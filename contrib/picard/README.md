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

Then enable **musefs sync** in Options → Plugins, and configure it in
Options → musefs sync:

- **musefs DB path** — path to the musefs SQLite store (required).
- **musefs binary** — the `musefs` executable (PATH name or full path), used to
  auto-create rows. Default `musefs`.
- **Run `musefs scan` before syncing** — autoscan toggle (default on). With it
  off, run `musefs scan` yourself first or the sync errors on a missing DB.
- **Extra field map** — optional `key=value` list mapping extra Picard tag names
  to musefs keys, e.g. `comment=comment`.

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

### Manual smoke test (the GUI path is not unit-tested)

1. `cargo build` and create a store: `musefs scan /path/to/album --db /tmp/m.db`.
2. Copy the plugin into Picard's plugins dir; enable it; set DB path `/tmp/m.db`.
3. Load the album in Picard, change a tag (e.g. title), add a front cover.
4. Right-click → **Sync to musefs**; confirm the status bar / log reports
   `synced=N`.
5. `musefs mount /tmp/mnt --db /tmp/m.db` and verify the mounted file carries the
   new tag and cover, with byte-identical audio.
