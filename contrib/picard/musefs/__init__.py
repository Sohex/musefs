"""musefs Picard plugin: sync Picard metadata into the musefs SQLite store.

Right-click selected files/albums/clusters → "Sync to musefs". The plugin
runs `musefs scan` on each file (autoscan) to create/refresh its track row,
then writes Picard's tags + front cover into the store keyed by realpath. The
audio file is never saved by Picard, preserving musefs's no-rewrite invariant.

All logic lives in musefs._core (unit-tested); this module is a thin Picard
adapter, verified by the README's manual smoke test (spec §10.2).
"""

from __future__ import annotations

import os
from functools import partial

from musefs._common import (
    Record,
    ScanError,
    check_schema_version,
    connect,
    realpath_key,
    run_scan,
    sync_files,
)
from musefs._core import (
    SCAN_TIMEOUT_SECONDS,
    MusefsError,
    front_cover,
    map_fields,
    resolve_config,
)

PLUGIN_NAME = "musefs sync"
PLUGIN_AUTHOR = "musefs contributors"
PLUGIN_DESCRIPTION = (
    "Right-click a file/album → 'Sync to musefs' to push Picard's tags and "
    "front cover into a musefs SQLite store, without rewriting the audio file."
)
PLUGIN_VERSION = "0.1.0"
# Floor: 2.0 — all required APIs (BaseAction, register_*_action, OptionsPage,
# register_options_page, config.TextOption/BoolOption, thread.run_task,
# iterfiles, metadata.images, is_front_image) are present since Picard 2.0.0.
PLUGIN_API_VERSIONS = [
    "2.0",
    "2.1",
    "2.2",
    "2.3",
    "2.4",
    "2.5",
    "2.6",
    "2.7",
    "2.8",
    "2.9",
    "2.10",
    "2.11",
    "2.12",
    "2.13",
]
PLUGIN_LICENSE = "MIT"
PLUGIN_LICENSE_URL = "https://opensource.org/licenses/MIT"

try:
    from picard import config, log
    from picard.ui.itemviews import (
        BaseAction,
        register_album_action,
        register_cluster_action,
        register_file_action,
        register_track_action,
    )
    from picard.ui.options import OptionsPage, register_options_page
    from picard.util import thread

    _PICARD = True
except ImportError:  # Running the unit tests without Picard installed.
    _PICARD = False


if _PICARD:
    # Option keys (also the names registered on the options page).
    OPT_DB = "musefs_db"
    OPT_BIN = "musefs_bin"
    OPT_AUTOSCAN = "musefs_autoscan"
    OPT_FIELDS = "musefs_fields"

    def _resolved_files(objs):
        """Resolve a selection (File/Track/Album/Cluster) to a dict of
        realpath-key -> File, de-duplicated. Picard items all implement
        iterfiles(); a File yields itself; a matched Track with no on-disk
        file yields nothing."""
        seen = {}
        for obj in objs:
            for f in obj.iterfiles():
                if not f.filename:  # unsaved/virtual file: no path to key on
                    continue
                seen.setdefault(realpath_key(f.filename), f)
        return seen

    def _scan_error(exc):
        """Translate a python-musefs ScanError to MusefsError, preserving the
        plugin's historical message text."""
        if exc.kind == "not_found":
            return MusefsError(
                f"musefs binary '{exc.binary}' not found; set the binary path "
                f"in the musefs options"
            )
        if exc.kind == "timeout":
            return MusefsError(
                f"`{exc.binary} scan` for {exc.target} timed out after "
                f"{SCAN_TIMEOUT_SECONDS}s; the scan may be stuck — check the "
                f"binary and DB."
            )
        return MusefsError(
            f"`{exc.binary} scan` failed for {exc.target} "
            f"(exit {exc.returncode}): {exc.stderr}"
        )

    def _do_sync(opts, files):
        """Background-thread worker: autoscan each file, then write tags/art.
        Returns SyncStats. Raises MusefsError / SchemaMismatch on hard failure."""
        if not opts.db:
            raise MusefsError("no musefs DB configured; set the DB path in Options → musefs sync")
        if opts.autoscan:
            for f in files.values():
                try:
                    run_scan(opts.bin, opts.db, f.filename, timeout=SCAN_TIMEOUT_SECONDS)
                except ScanError as exc:
                    raise _scan_error(exc)
        elif not os.path.exists(opts.db):
            raise MusefsError(
                f"musefs DB not found at {opts.db}; enable autoscan or run `musefs scan` first"
            )

        conn = connect(opts.db)
        try:
            check_schema_version(conn)
            records = []
            for key, f in files.items():
                pairs = map_fields(f.metadata, opts.fields)
                art = front_cover(f.metadata)
                records.append(Record(key=key, pairs=pairs, art=art))
            stats = sync_files(conn, records)
            conn.commit()
            return stats
        finally:
            conn.close()

    class MusefsSync(BaseAction):
        NAME = "Sync to musefs"

        def callback(self, objs):
            files = _resolved_files(objs)
            if not files:
                self._status("musefs: nothing to sync (no on-disk files selected)")
                return
            # Build a plain dict from Picard's config (subscriptable per
            # registered option) so resolve_config keeps its tested dict
            # contract rather than depending on config.setting's API.
            settings = {
                OPT_DB: config.setting[OPT_DB],
                OPT_BIN: config.setting[OPT_BIN],
                OPT_AUTOSCAN: config.setting[OPT_AUTOSCAN],
                OPT_FIELDS: config.setting[OPT_FIELDS],
            }
            opts = resolve_config(settings, os.environ)
            thread.run_task(
                partial(_do_sync, opts, files),
                partial(self._done, len(files)),
            )

        def _done(self, n_files, result=None, error=None):
            if error is not None:
                log.error("musefs: sync failed: %s", error)
                self._status(f"musefs: sync failed: {error}")
                return
            stats = result
            log.info("musefs: %s (files=%d)", stats.summary(), n_files)
            self._status(f"musefs: {stats.summary()}")

        @staticmethod
        def _status(message):
            # Logging is the reliable cross-version channel. Picard's status-bar
            # API varies by version (e.g. tagger.window.set_statusbar_message);
            # a future change could also surface this on-screen.
            log.info("%s", message)

    class MusefsOptionsPage(OptionsPage):
        NAME = "musefs_sync"
        TITLE = "musefs sync"
        PARENT = "plugins"

        options = [
            config.TextOption("setting", OPT_DB, ""),
            config.TextOption("setting", OPT_BIN, "musefs"),
            config.BoolOption("setting", OPT_AUTOSCAN, True),
            config.TextOption("setting", OPT_FIELDS, ""),
        ]

        def __init__(self, parent=None):
            super().__init__(parent)
            from PyQt5 import QtWidgets

            layout = QtWidgets.QFormLayout(self)
            self._db = QtWidgets.QLineEdit(self)
            self._bin = QtWidgets.QLineEdit(self)
            self._autoscan = QtWidgets.QCheckBox("Run `musefs scan` before syncing", self)
            self._fields = QtWidgets.QLineEdit(self)
            self._fields.setPlaceholderText("extra map, e.g. comment=comment, mood=mood")
            layout.addRow("musefs DB path", self._db)
            layout.addRow("musefs binary", self._bin)
            layout.addRow("", self._autoscan)
            layout.addRow("Extra field map", self._fields)

        def load(self):
            self._db.setText(config.setting[OPT_DB])
            self._bin.setText(config.setting[OPT_BIN])
            self._autoscan.setChecked(config.setting[OPT_AUTOSCAN])
            self._fields.setText(config.setting[OPT_FIELDS])

        def save(self):
            config.setting[OPT_DB] = self._db.text().strip()
            config.setting[OPT_BIN] = self._bin.text().strip() or "musefs"
            config.setting[OPT_AUTOSCAN] = self._autoscan.isChecked()
            config.setting[OPT_FIELDS] = self._fields.text().strip()

    _action = MusefsSync()
    register_file_action(_action)
    register_track_action(_action)
    register_album_action(_action)
    register_cluster_action(_action)
    register_options_page(MusefsOptionsPage)
