"""beets plugin: sync canonical beets metadata into the musefs SQLite store."""

import os
import sqlite3
import subprocess

from beets import ui
from beets.plugins import BeetsPlugin
from musefs_common import (
    SCAN_TIMEOUT_SECONDS,
    ScanError,
    SchemaMismatch,
    SyncStats,
    check_schema_version,
    connect,
    prune_missing,
    realpath_key,
    run_scan,
    sync_files,
    track_id_for_path,
)

from beetsplug import _core


class MusefsPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()
        self.config.add({
            "db": None,
            "fields": {},
            "bin": "musefs",  # musefs executable (PATH name or full path)
            "autoscan": True,  # run `musefs scan` automatically before syncing
            "write_path": True,  # emit a beets_path tag for $!{beets_path} mounts
        })
        # beets has no file-move event, and `after_write` fires *before* a move
        # (at the old path). So imports/writes are recorded and reconciled once
        # at cli_exit, when each item's path is final, where we also prune rows
        # whose backing file has moved away.
        self._pending = []
        self.register_listener("after_write", self._record)
        self.register_listener("item_imported", self._record)
        self.register_listener("album_imported", self._record_album)
        self.register_listener("cli_exit", self._reconcile_pending)

    # --- command ---------------------------------------------------------

    def commands(self):
        cmd = ui.Subcommand("musefs", help="sync beets metadata into the musefs DB")
        cmd.parser.add_option(
            "--db",
            dest="db",
            default=None,
            help="path to the musefs SQLite store (overrides config)",
        )
        cmd.parser.add_option(
            "-n",
            "--dry-run",
            dest="dry_run",
            action="store_true",
            default=False,
            help="report what would change without writing",
        )
        cmd.func = self._command
        return [cmd]

    @staticmethod
    def _query_from_args(args):
        """Drop an optional leading `sync` verb so `beet musefs sync QUERY`
        and `beet musefs QUERY` both work."""
        if args and args[0] == "sync":
            return list(args[1:])
        return list(args)

    def _command(self, lib, opts, args):
        db_path = opts.db or self._db_path()
        if not db_path:
            raise ui.UserError("musefs: set `musefs.db` in config or pass --db")

        query = self._query_from_args(args)
        items = list(lib.items(query))
        if self._autoscan() and not opts.dry_run:
            # Full sync: one scan of the music dir. Query: scan only the matched
            # files, so non-matched rows aren't re-seeded from their files.
            targets = (
                [os.fsdecode(i.path) for i in items] if query else [os.fsdecode(lib.directory)]
            )
            self._run_scan(db_path, targets)
        stats = self._sync(db_path, items, dry_run=opts.dry_run)
        if opts.dry_run:
            pruned = 0
        else:
            prune_items = items if query else None
            pruned = self._prune_missing(db_path, items=prune_items)
        # ui.print_ (not self._log) so the summary always shows, not only at -v.
        ui.print_(f"musefs: {stats.summary()} pruned={pruned}")

    # --- event listeners -------------------------------------------------

    def _record(self, item=None, **kwargs):
        if item is not None:
            self._pending.append(item)

    def _record_album(self, album=None, **kwargs):
        if album is not None:
            self._pending.extend(album.items())

    def _reconcile_pending(self, lib=None, **kwargs):
        """End-of-command reconcile: sync every touched item at its final path,
        then prune rows whose backing file moved away. Best-effort — a passive
        hook must never abort the beets operation, so errors become warnings."""
        pending, self._pending = self._pending, []
        # Dedup by final on-disk path (an item may fire several events).
        items = list({os.fsdecode(i.path): i for i in pending if i is not None}.values())
        if not items:
            return
        db_path = self._db_path()
        if not db_path:
            self._log.warning("musefs: no `musefs.db` configured; skipping sync")
            return
        try:
            if self._autoscan():
                self._run_scan(db_path, [os.fsdecode(i.path) for i in items])
            self._sync(db_path, items)
            self._prune_missing(db_path)
        except (ui.UserError, sqlite3.Error, OSError, subprocess.SubprocessError) as exc:
            # A passive cli_exit hook must never abort the beets operation for an
            # environmental failure (locked DB, vanished file, wedged scan); those
            # degrade to a warning. An unexpected exception still propagates so a
            # real bug surfaces instead of hiding behind a one-line warning.
            self._log.warning("musefs: {}", exc)

    # --- helpers ---------------------------------------------------------

    def _db_path(self):
        # `.get()` returns the raw config value (None if unset); only call
        # as_filename() when set, so a genuine bad-type value still raises.
        if self.config["db"].get() is None:
            return None
        return self.config["db"].as_filename()

    def _fields(self):
        return self.config["fields"].get(dict) or {}

    def _autoscan(self):
        return bool(self.config["autoscan"].get(bool))

    def _write_path(self):
        return bool(self.config["write_path"].get(bool))

    def _bin(self):
        return self.config["bin"].get(str) or "musefs"

    def _run_scan(self, db_path, targets):
        """Run `musefs scan <target...> --db <db>` once for the whole batch.
        Creates the DB if missing and fills the structural columns the plugin
        can't compute itself. Raises ui.UserError on failure."""
        binary = self._bin()
        try:
            run_scan(binary, db_path, targets, timeout=SCAN_TIMEOUT_SECONDS)
        except ScanError as exc:
            raise self._scan_user_error(exc)

    @staticmethod
    def _scan_user_error(exc):
        """Translate a python-musefs ScanError to beets' ui.UserError, preserving
        the plugin's historical message text."""
        if exc.kind == "not_found":
            return ui.UserError(
                f"musefs: binary '{exc.binary}' not found; set `musefs.bin` to "
                f"the musefs executable path"
            )
        return ui.UserError(
            f"musefs: `{exc.binary} scan` failed for {exc.target} "
            f"(exit {exc.returncode}):\n{exc.stderr}"
        )

    @staticmethod
    def _track_ids_for_items(conn, items):
        ids = []
        for item in items:
            key = realpath_key(item.path)
            track_id = track_id_for_path(conn, key)
            if track_id is not None:
                ids.append(track_id)
        return ids

    def _prune_missing(self, db_path, items=None):
        """Drop rows whose backing file no longer exists (moved/deleted).
        When ``items`` is provided, only their musefs track rows are checked.
        Returns the number pruned."""
        if not os.path.exists(db_path):
            return 0
        conn = connect(db_path)
        try:
            track_ids = None if items is None else self._track_ids_for_items(conn, items)
            pruned = prune_missing(conn, track_ids)
            conn.commit()
            return pruned
        finally:
            conn.close()

    def _sync(self, db_path, items, dry_run=False):
        if not os.path.exists(db_path):
            raise ui.UserError(
                f"musefs: DB not found at {db_path}; enable `musefs.autoscan` "
                f"or run `musefs scan` first"
            )
        conn = connect(db_path)
        try:
            check_schema_version(conn)
            stats = SyncStats()
            records = _core.build_records(
                items,
                fields=self._fields(),
                stats=stats,
                write_path=self._write_path(),
                log=self._log,
            )
            sync_files(conn, records, dry_run=dry_run, stats=stats)
            if dry_run:
                conn.rollback()
            else:
                conn.commit()
            return stats
        except SchemaMismatch as exc:
            conn.rollback()
            raise ui.UserError(f"musefs: {exc}")
        finally:
            conn.close()
