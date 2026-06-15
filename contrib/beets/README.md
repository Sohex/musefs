# beets-musefs

A [beets](https://beets.io) plugin that syncs your beets metadata (tags + cover
art) into a [musefs](../../README.md) SQLite store, so a live musefs mount shows
a re-tagged view of your library without rewriting any audio.

## How it fits together

- The plugin owns the **tags** (and **cover art**, when beets has it) of each
  track, keyed by the file's canonical real path.
- The structural columns (audio offsets, size, mtime) can only come from musefs
  probing the file, so the plugin runs `musefs scan` for you (via the `bin`
  config) before syncing — it never tries to compute those itself.
- `beet musefs` scans the library and then syncs; the import/write hooks scan
  just the touched file and then sync. musefs's auto-refresh shows changes live —
  no remount, and **no separate scan step**.

## Install

```bash
pip install beets-musefs
```

This pulls in the shared [`python-musefs`](../python-musefs/README.md) runtime
library from PyPI automatically — both packages are published, so no
working-tree install is needed.

### Use via pluginpath (no package install)

The plugin itself doesn't need to be installed — point beets at the plugin's
`beetsplug` directory and it loads at runtime. It still needs the shared
`python-musefs` runtime library importable, so install that first:

```bash
pip install python-musefs
```

beets adds `pluginpath` entries directly to the `beetsplug` package path, so it
must be the `beetsplug` dir itself (not its parent). In your beets `config.yaml`:

```yaml
pluginpath: /path/to/musefs/contrib/beets/beetsplug
plugins: musefs
musefs:
  db: ~/musefs.db          # path to the musefs SQLite store (required)
  bin: musefs              # musefs executable for auto-scan; use a full path if
                           # not on $PATH, e.g. /path/to/musefs/target/release/musefs
  # autoscan: yes          # default; runs `musefs scan` for you. Set `no` to
  #                        # manage scanning yourself (hooks then best-effort).
  # fields:                # optional: map extra beets fields to musefs keys
  #   comments: comment
```

### Development install (from a checkout)

To hack on the plugin or run the test suite against your working tree, install
both packages editable from the repo so imports resolve to the local source:

```bash
pip install -e contrib/python-musefs       # shared library
pip install -e "contrib/beets[test]"       # plugin + test deps
```

## Workflow (test drive)

```bash
# Sync beets metadata into the store. Auto-scans the library first (creating the
# DB if needed) — no separate `musefs scan` step.
beet musefs                      # everything
beet musefs albumartist:"Boards of Canada"   # a subset (scans just those files)
beet musefs -n                   # dry run: report counts, write nothing

# Mount the re-tagged view.
musefs mount ~/mnt --db ~/musefs.db \
    --template '$albumartist/$album/$tracknumber - $title'

# ...or mirror your beets library layout exactly, via the computed beets_path tag.
musefs mount ~/mnt --db ~/musefs.db --template '$!{beets_path}'
```

Imports and tag write-backs auto-sync via event hooks: `beet import` and
`beet modify -w …` record the touched items and reconcile them once the command
finishes — when each file's path is final (beets has no move event, and a write
fires *before* its move). The reconcile scans the new path and prunes the row
left behind at the old one. A metadata-only `beet modify` (no `-w`) doesn't fire
a hook — re-run `beet musefs`. With `autoscan: no`, run `musefs scan` yourself
first; the hooks then skip gracefully if the DB is missing.

## Never writing to your backing audio files

If your backing files must stay byte-for-byte untouched — you're seeding them as
a torrent, the library is immutable, or you simply want beets to drive the musefs
view without ever rewriting a tag — configure beets to never write to disk:

```yaml
import:
    copy: no
    move: no
    write: no
```

`write: no` is enough on its own: every stock beets plugin gates its file writes
on `import.write`. The musefs plugin reads canonical metadata from the **beets
database**, not from the files, and `musefs scan` ingests/synthesizes embedded
art itself — so `write: no` loses nothing in the musefs view.

A few plugins ignore that gate or are redundant in this mode:

- **scrub** — deletes all tags from files directly via mutagen, *ignoring*
  `import.write`; its auto-import hook would wipe tags from your backing files.
  Don't enable it.
- **embedart** — embeds cover art into the audio files. Redundant: musefs already
  presents embedded art in the virtual files (scan ingestion plus the plugin's
  overlay of the album's `artpath`).
- **zero** — only acts during a file write, so it is inert with `write: no`
  (nothing to do, but nothing to worry about either).

## Notes

- **Field coverage:** every tag beets writes to a file (its `_media_tag_fields`)
  is synced — ReplayGain, MusicBrainz IDs, comment, lyrics, grouping, isrc,
  multi-valued artists, and any custom field — under canonical musefs keys.
  Read-only file facts (bitrate, length, …) are never written as tags.
- **Merge, not replace:** beets' values win for the fields it manages; any other
  tag already embedded in the file is preserved in the view.
- **Deletions stick:** the plugin records the keys it manages per track in a
  `musefs_managed` beets flexattr (stored in the beets DB only — never in your
  audio files or the musefs store). Remove a tag in beets and it is removed from
  the view and stays gone across re-scans.
- **`--restore-backing`** (or `restore_backing: yes`): when you remove a tag in
  beets, let the file's original embedded value reappear instead of disappearing.
- **Caveat:** sticky deletion relies on `autoscan: yes` (the default), which
  re-derives the file's embedded tags before each sync. With `autoscan: no`, a
  deletion only takes effect after your next manual `musefs scan`.
- **Cover art:** taken from the album's `artpath` (beets' external cover file).
  beets art wins when present; otherwise any art `musefs scan` ingested from
  embedded pictures is preserved.
- **Computed path (`beets_path`):** each sync also writes a `beets_path` text tag
  holding the track's beets library-relative path (from your `paths:` config, via
  `item.destination`), with the file extension removed — musefs re-appends it. Mount
  with `--template '$!{beets_path}'` (the `$!{}` path field keeps `/` as directory
  separators) to mirror your beets layout, including layouts musefs's own template
  engine can't express. Set `write_path: no` in the `musefs:` config to skip it.
  Do not add an extension in a template that consumes `beets_path`. See the
  computed-tag workflow in [ARCHITECTURE.md](../../ARCHITECTURE.md).
- **Moves & deletes:** every sync (the command and the end-of-command reconcile)
  prunes track rows whose backing file is no longer present, so renames/moves
  don't leave stale entries. Caveat: a file that's merely offline at sync time
  (e.g. an unmounted network share) is also pruned — sync while the library is
  available.
- **Removals prune the store.** `beet remove -d` deletes the backing file, so the
  store row is pruned at the end of the command. A bare `beet remove` (which keeps
  the file on disk) leaves the row in place — musefs can still serve those bytes.
- **Orphaned art:** replacing art can orphan old blobs; `musefs scan --revalidate`
  garbage-collects them.
- **Schema version:** the plugin refuses to run if the DB's `user_version` differs
  from the version it targets — rebuild after upgrading musefs.

## Tests

The tests live under `tests/` and use a local virtualenv with beets + pytest.

```bash
cd contrib/beets
uv venv                                   # create .venv (once)
source .venv/bin/activate
uv pip install -e ../python-musefs        # shared library (editable, from the working tree)
uv pip install -r requirements.txt        # beets + pytest

python -m pytest                          # unit + integration (no Rust binary)
python -m pytest -m musefs_bin            # path-matching gate vs the real `musefs` binary
python -m pytest -m e2e                   # full beets -> mount -> playback end-to-end
```

The `musefs_bin` gate shells out to the real `musefs` binary, so build it first
from the repo root (`cargo build`) and run it against a fresh build. The `e2e`
tier additionally needs `ffmpeg` and `/dev/fuse` + `fusermount`: it generates
audio, imports it with beets, retags, syncs, mounts via FUSE, and verifies the
mount's tags and byte-identical audio (including a move-reconcile case). Both
tiers are deselected from the default run and skip cleanly if their tools are
absent.
