# musefs-picard

A [MusicBrainz Picard](https://picard.musicbrainz.org/) plugin that syncs your
Picard metadata (tags + front cover) into a [musefs](../../README.md) SQLite
store, so a live musefs mount shows a re-tagged view of your library **without
rewriting any audio**.

## How it fits together

Picard has no way to redirect its Save to a database, so this plugin adds a
**context-menu action** instead: match/edit as usual, then right-click your
selection → **"Sync to musefs"** *instead of* pressing Save. The plugin:

1. runs `musefs scan` on each selected file to create/refresh its track row and
   structural columns (the offsets only musefs can compute), then
2. writes Picard's tags and front cover into the store, keyed by the file's
   canonical real path.

musefs's auto-refresh surfaces the change at the mount with no remount. The
audio file is never saved by Picard.

## Install (local / development)

Picard loads "folder plugins" from its plugins directory. Copy (or symlink) the
`musefs/` folder there:

- Linux: `~/.config/MusicBrainz/Picard/plugins/`
- macOS: `~/Library/Preferences/MusicBrainz/Picard/plugins/`
- Windows: `%APPDATA%\MusicBrainz\Picard\plugins\`

```bash
cp -r contrib/picard/musefs ~/.config/MusicBrainz/Picard/plugins/
```

The `musefs/_common/` subfolder is the vendored `python-musefs` library, copied
in so the plugin folder is self-contained (Picard does not install plugin
dependencies). It is committed; you don't need to do anything to use it. If you
change the shared library, re-run `python contrib/python-musefs/vendor_to_picard.py`
and commit the refreshed copy — CI's drift guard enforces it.

Then enable **musefs sync** in Options → Plugins, and configure it in
Options → musefs sync:

- **musefs DB path** — path to the musefs SQLite store (required).
- **musefs binary** — the `musefs` executable (PATH name or full path), used to
  auto-create rows. Default `musefs`.
- **Run `musefs scan` before syncing** — autoscan toggle (default on). With it
  off, run `musefs scan` yourself first or the sync errors on a missing DB.
- **Extra field map** — optional `key=value` list mapping additional or custom
  Picard tag names to musefs store keys (applied verbatim, last-wins, on top of
  the automatic full-tag-set sync), e.g. `mymood=mood`.

`MUSEFS_DB` and `MUSEFS_BIN` environment variables override the DB/binary
settings (handy for testing).

## Workflow

1. `musefs mount ~/mnt --db ~/musefs.db --template '$albumartist/$album/$tracknumber - $title'`
2. In Picard, match/cluster an album as usual.
3. Right-click the album/files → **Sync to musefs**.
4. Browse `~/mnt` — the files show Picard's tags and cover, audio byte-identical.

## Notes

- **Front cover only:** the first front-cover image Picard holds is synced.
  Picard art wins when present; otherwise any art `musefs scan` ingested from
  the file's embedded picture is preserved. Re-syncing a file with no Picard
  art lets the embedded picture re-seed when autoscan is on (musefs scan
  re-reads the file); with autoscan off, existing art is left untouched.
- **Tags are fully replaced** with Picard's view on every sync.
- **Field coverage:** every populated Picard tag is synced under its canonical
  musefs (on-disk) key — all MusicBrainz IDs, sort and performer/credit fields,
  movement, totals, and any custom field; multi-values expand and per-role
  performers fold to `Name (Role)`. Picard's hidden `~` internals (length,
  rating, …) are never written.
- **Orphaned art:** replacing art can orphan old blobs; `musefs scan --revalidate`
  garbage-collects them.
- **Schema version:** the plugin refuses to run if the DB's `user_version`
  differs from the version it targets — rebuild the store after upgrading musefs.

## Tests

```bash
cd contrib/picard
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt

python -m pytest                 # unit + integration (no Picard, no Rust binary)
python -m pytest -m musefs_bin   # path-matching gate vs the real `musefs` binary
```

The `musefs_bin` gate shells out to the real `musefs` binary, so build it first
from the repo root (`cargo build`). It is deselected from the default run and
skips cleanly if the binary is absent.

### Real-Picard (pytest-qt) tests

The adapter (`musefs/__init__.py`) is exercised against a real Picard + PyQt5
install, headless. Picard isn't a clean pip wheel, so use the distro package and
bind a uv venv to the system Python it targets:

```bash
sudo apt-get install -y picard                              # Picard at /usr/lib/picard + system PyQt5
uv venv --system-site-packages --python "$(which python3)"  # match apt Picard's C-ext interpreter
uv pip install -e 'contrib/picard[test]'                    # test extra includes pytest-qt
PYTHONPATH=/usr/lib/picard QT_QPA_PLATFORM=offscreen \
  .venv/bin/python -m pytest contrib/picard/tests -v
```

These tests `importorskip("picard")`, so on a machine without Picard they skip
cleanly and only the Qt-free `_core` tests run.

### Manual smoke test (full GUI round-trip)

1. `cargo build` and create a store: `musefs scan /path/to/album --db /tmp/m.db`.
2. Copy the plugin into Picard's plugins dir; enable it; set DB path `/tmp/m.db`.
3. Load the album in Picard, change a tag (e.g. title), add a front cover.
4. Right-click → **Sync to musefs**; confirm the status bar / log reports
   `synced=N`.
5. `musefs mount /tmp/mnt --db /tmp/m.db` and verify the mounted file carries the
   new tag and cover, with byte-identical audio.
