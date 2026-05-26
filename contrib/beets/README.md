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

## Install (local / development)

No install needed — point beets at the plugin's `beetsplug` directory. beets adds
`pluginpath` entries directly to the `beetsplug` package path, so it must be the
`beetsplug` dir itself (not its parent). In your beets `config.yaml`:

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
```

Imports and tag write-backs auto-sync via event hooks: `beet import` and
`beet modify -w …` record the touched items and reconcile them once the command
finishes — when each file's path is final (beets has no move event, and a write
fires *before* its move). The reconcile scans the new path and prunes the row
left behind at the old one. A metadata-only `beet modify` (no `-w`) doesn't fire
a hook — re-run `beet musefs`. With `autoscan: no`, run `musefs scan` yourself
first; the hooks then skip gracefully if the DB is missing.

## Notes

- **Cover art:** taken from the album's `artpath` (beets' external cover file).
  beets art wins when present; otherwise any art `musefs scan` ingested from
  embedded pictures is preserved.
- **Moves & deletes:** every sync (the command and the end-of-command reconcile)
  prunes track rows whose backing file is no longer present, so renames/moves
  don't leave stale entries. Caveat: a file that's merely offline at sync time
  (e.g. an unmounted network share) is also pruned — sync while the library is
  available.
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
uv pip install -r requirements.txt        # beets + pytest

python -m pytest                          # unit + integration (no Rust binary)
python -m pytest -m musefs_bin            # path-matching gate vs the real `musefs` binary
```

The `musefs_bin` gate shells out to the real `musefs` binary, so build it first
from the repo root (`cargo build`) and run the gate against a fresh build.
