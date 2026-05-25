# beets-musefs

A [beets](https://beets.io) plugin that syncs your beets metadata (tags + cover
art) into a [musefs](../../README.md) SQLite store, so a live musefs mount shows
a re-tagged view of your library without rewriting any audio.

## How it fits together

- `musefs scan` owns track rows and the structural columns (audio offsets, size,
  mtime). Run it first; it also seeds tags/art from the files' embedded metadata.
- This plugin overwrites the **tags** (and **cover art**, when beets has it) of
  rows that scan already created, keyed by each file's canonical real path.
- musefs's auto-refresh picks the changes up live — no remount.

The plugin **never** creates rows or touches structural columns. A beets item
whose path wasn't scanned is reported as skipped.

## Install (local / development)

No install needed — point beets at this directory. In your beets `config.yaml`:

```yaml
pluginpath: /path/to/musefs/contrib/beets
plugins: musefs
musefs:
  db: ~/musefs.db          # path to the musefs SQLite store (required)
  # fields:                # optional: map extra beets fields to musefs keys
  #   comments: comment
```

## Workflow (test drive)

```bash
# 1. Probe the library; create rows + structural columns + seed metadata.
musefs scan ~/music --db ~/musefs.db

# 2. Overwrite tags/art from beets (whole library, or a query).
beet musefs                      # everything
beet musefs albumartist:"Boards of Canada"   # a subset
beet musefs -n                   # dry run: report counts, write nothing

# 3. Mount the re-tagged view.
musefs mount ~/mnt --db ~/musefs.db \
    --template '$albumartist/$album/$tracknumber - $title'
```

After this, the event hooks auto-sync on writes and imports specifically:
`beet modify -w …` (tags written back to the file) and `beet import`. The mount
then refreshes on its own. Note a metadata-only `beet modify` (no `-w`) does
**not** trigger a hook — re-run `beet musefs` to pick those edits up.

## Notes

- **Cover art:** taken from the album's `artpath` (beets' external cover file).
  beets art wins when present; otherwise any art `musefs scan` ingested from
  embedded pictures is preserved.
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
