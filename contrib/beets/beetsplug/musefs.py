"""beets plugin: sync canonical beets metadata into the musefs SQLite store."""

import os

from beets import ui
from beets.plugins import BeetsPlugin

from beetsplug import _core


class MusefsPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()
        self.config.add({"db": None, "fields": {}})
        self.register_listener("after_write", self._on_after_write)
        self.register_listener("item_imported", self._on_item_imported)
        self.register_listener("album_imported", self._on_album_imported)

    # --- command ---------------------------------------------------------

    def commands(self):
        cmd = ui.Subcommand("musefs", help="sync beets metadata into the musefs DB")
        cmd.parser.add_option(
            "--db", dest="db", default=None,
            help="path to the musefs SQLite store (overrides config)",
        )
        cmd.parser.add_option(
            "-n", "--dry-run", dest="dry_run", action="store_true", default=False,
            help="report what would change without writing",
        )
        cmd.func = self._command
        return [cmd]

    @staticmethod
    def _query_from_args(args):
        """Drop an optional leading `sync` verb so `beet musefs sync QUERY`
        and `beet musefs QUERY` both work."""
        if args and args[0] == "sync":
            return args[1:]
        return list(args)

    def _command(self, lib, opts, args):
        db_path = opts.db or self._db_path()
        if not db_path:
            raise ui.UserError("musefs: set `musefs.db` in config or pass --db")

        query = self._query_from_args(args)
        items = list(lib.items(query))
        stats = self._sync(db_path, items, dry_run=opts.dry_run)
        self._log.info("musefs: {}", stats.summary())

    # --- event listeners -------------------------------------------------

    def _on_after_write(self, item=None, path=None, **kwargs):
        self._sync_listener([item] if item is not None else [])

    def _on_item_imported(self, lib=None, item=None, **kwargs):
        self._sync_listener([item] if item is not None else [])

    def _on_album_imported(self, lib=None, album=None, **kwargs):
        if album is None:
            return
        self._sync_listener(list(album.items()))

    # --- helpers ---------------------------------------------------------

    def _db_path(self):
        # `.get()` returns the raw config value (None if unset); only call
        # as_filename() when set, so a genuine bad-type value still raises.
        if self.config["db"].get() is None:
            return None
        return self.config["db"].as_filename()

    def _fields(self):
        return self.config["fields"].get(dict) or {}

    def _sync_listener(self, items):
        """Sync a listener's affected items, skipping gracefully if unconfigured."""
        items = [i for i in items if i is not None]
        if not items:
            return
        db_path = self._db_path()
        if not db_path:
            self._log.warning("musefs: no `musefs.db` configured; skipping sync")
            return
        self._sync(db_path, items)

    def _sync(self, db_path, items, dry_run=False):
        if not os.path.exists(db_path):
            raise ui.UserError(
                f"musefs: DB not found at {db_path}; run `musefs scan` first"
            )
        conn = _core.connect(db_path)
        try:
            _core.check_schema_version(conn)
            stats = _core.sync_items(
                conn, items, fields=self._fields(), dry_run=dry_run
            )
            if dry_run:
                conn.rollback()
            else:
                conn.commit()
            return stats
        except _core.SchemaMismatch as exc:
            conn.rollback()
            raise ui.UserError(f"musefs: {exc}")
        finally:
            conn.close()
