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
    run_scan,
    sync_files,
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
            "restore_backing": False,  # on delete, let the backing tag value reappear
        })
        # beets has no file-move event, and `after_write` fires *before* a move
        # (at the old path). So imports/writes are recorded and reconciled once
        # at cli_exit, when each item's path is final. Reconcile only syncs; it
        # never prunes. Pruning is a deliberate act owned by `musefs revalidate
        # --prune` (reachable via `beet musefs --revalidate`).
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
        cmd.parser.add_option(
            "--restore-backing",
            dest="restore_backing",
            action="store_true",
            default=False,
            help="when a tag is removed in beets, let the backing file's value reappear",
        )
        cmd.parser.add_option(
            "--revalidate",
            dest="revalidate",
            action="store_true",
            default=False,
            help="forward to the `musefs revalidate` subcommand, pruning rows "
            "whose backing file is gone and GCing orphaned art (the only way "
            "this plugin prunes)",
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
        revalidate = bool(opts.revalidate)
        # A scan runs when autoscan is on, or whenever --revalidate is requested:
        # revalidation is the pruning maintenance pass (`musefs revalidate
        # --prune`). The plugin never prunes on its own; this is the only path
        # that removes rows.
        if (self._autoscan() or revalidate) and not opts.dry_run:
            # Full sync: one scan of the music dir. Query: scan only the matched
            # files, so non-matched rows aren't re-seeded from their files.
            targets = (
                [os.fsdecode(i.path) for i in items] if query else [os.fsdecode(lib.directory)]
            )
            if revalidate:
                self._run_scan(db_path, targets, revalidate=True, prune=True)
            else:
                self._run_scan(db_path, targets, force=True)
        restore_backing = bool(opts.restore_backing) or self._restore_backing()
        stats = self._sync(db_path, items, dry_run=opts.dry_run, restore_backing=restore_backing)
        # ui.print_ (not self._log) so the summary always shows, not only at -v.
        ui.print_(f"musefs: {stats.summary()}")

    # --- event listeners -------------------------------------------------

    def _record(self, item=None, **kwargs):
        if item is not None:
            self._pending.append(item)

    def _record_album(self, album=None, **kwargs):
        if album is not None:
            self._pending.extend(album.items())

    def _reconcile_pending(self, lib=None, **kwargs):
        """End-of-command reconcile: sync every touched item at its final path.
        Best-effort — a passive hook must never abort the beets operation, so
        errors become warnings.

        It never prunes. Pruning is a deliberate act (#538): an unscoped
        existence-based prune at every ``cli_exit`` would mass-delete plugin
        metadata on a transient backing-storage loss (an unmounted share or a
        momentary realpath divergence). Removing rows for moved-away/deleted
        files is left to the explicit ``beet musefs`` command (and ``musefs
        scan``)."""
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
                self._run_scan(db_path, [os.fsdecode(i.path) for i in items], force=True)
            self._sync(db_path, items, restore_backing=self._restore_backing())
        except (ui.UserError, sqlite3.Error, OSError, subprocess.SubprocessError) as exc:
            # A passive cli_exit hook must never abort the beets operation for an
            # environmental failure (locked DB, vanished file, wedged scan); those
            # degrade to a warning. An unexpected exception still propagates so a
            # real bug surfaces instead of hiding behind a one-line warning.
            if self._is_permission_error(exc):
                # A persistent setup failure (read-only DB / permission denied)
                # would otherwise be a silent no-op: beets hides plugin WARNINGs
                # at default verbosity, so the user gets no sign the sync did
                # nothing. Surface it via ui.print_ — but still don't abort.
                ui.print_(
                    f"musefs: cannot write {db_path} (read-only/permission denied) "
                    f"— metadata not synced"
                )
            else:
                self._log.warning("musefs: {}", exc)

    @staticmethod
    def _is_permission_error(exc):
        """True if ``exc`` is a persistent permission/read-only DB failure (worth
        surfacing loudly), as opposed to a transient lock or a vanished file."""
        if isinstance(exc, PermissionError):
            return True
        if isinstance(exc, sqlite3.OperationalError):
            msg = str(exc).lower()
            return "readonly database" in msg or "unable to open database file" in msg
        return False

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

    def _restore_backing(self):
        return bool(self.config["restore_backing"].get(bool))

    def _bin(self):
        return self.config["bin"].get(str) or "musefs"

    def _run_scan(self, db_path, targets, *, revalidate=False, force=False, prune=False):
        """Run musefs once for the whole batch.

        ``force`` re-seeds existing tracks from their backing files. ``revalidate``
        forwards to the new subcommand; ``prune`` deletes gone rows and GCs
        orphaned art."""
        binary = self._bin()
        try:
            run_scan(
                binary,
                db_path,
                targets,
                revalidate=revalidate,
                force=force,
                prune=prune,
                timeout=SCAN_TIMEOUT_SECONDS,
            )
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

    def _sync(self, db_path, items, dry_run=False, restore_backing=False):
        if not os.path.exists(db_path):
            raise ui.UserError(
                f"musefs: DB not found at {db_path}; enable `musefs.autoscan` "
                f"or run `musefs scan` first"
            )
        conn = connect(db_path)
        try:
            check_schema_version(conn)
            stats = SyncStats()
            records, managed_writes = _core.build_records(
                items,
                fields=self._fields(),
                stats=stats,
                write_path=self._write_path(),
                restore_backing=restore_backing,
                log=self._log,
            )
            sync_files(conn, records, dry_run=dry_run, stats=stats, merge=True)
            if dry_run:
                conn.rollback()
            else:
                conn.commit()
                _core.persist_managed(managed_writes)
            return stats
        except SchemaMismatch as exc:
            conn.rollback()
            raise ui.UserError(f"musefs: {exc}")
        finally:
            conn.close()
