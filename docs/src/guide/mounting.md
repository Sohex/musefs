# Mounting & path templates

## Mount

```bash
musefs mount /path/to/mountpoint --db library.db \
    --template '$albumartist/$album/$title' \
    --default-fallback Unknown \
    --fallback albumartist='Unknown Artist' \
    --mode synthesis        # or: structure-only
```

`mount` blocks until the filesystem is unmounted (`fusermount3 -u`, or
Ctrl-C).

> **`mount` never creates the store** — unlike `scan`, it requires a populated
> DB to already exist and exits non-zero otherwise. Interactively this is
> invisible (the `scan` → `mount` quick start always seeds it first), but it
> bites automation: a `mount` started at boot before anything has scanned
> hard-fails (and crash-loops under `Restart=`). Seed the store with an initial
> `scan`, or order the mount after it — see
> [`contrib/systemd`](../integrations/systemd.md).

> **Mounting at an arbitrary path may be denied by AppArmor.** On distros that
> ship an AppArmor profile for `fusermount3` (Ubuntu 24.04+ / libfuse ≥ 3.17),
> unprivileged FUSE mounts are only allowed when the mountpoint is under a
> whitelisted prefix — the shipped profile permits `$HOME/**`, `/mnt`, `/media`,
> `/tmp`, `/cvmfs`, `$XDG_RUNTIME_DIR`, plus flatpak dirs. Mounting elsewhere
> (e.g. a data volume at `/data/...`) fails with `fusermount3: mount failed:
> Permission denied`, and the kernel audit log shows
> `apparmor="DENIED" operation="mount" … profile="fusermount3"`. The mountpoint's
> own ownership is irrelevant — AppArmor rejects the `mount()` syscall first. Fix
> it by mounting under a permitted prefix, or by whitelisting your prefix in
> `/etc/apparmor.d/local/fusermount3` (the shipped profile ends with
> `include if exists <local/fusermount3>`).

Two modes:

- **`synthesis`** (default) — files carry metadata freshly generated from
  the store, spliced ahead of the original audio bytes.
- **`structure-only`** — files are served byte-for-byte as they are on disk;
  only the directory tree is virtual.

Edit tags or art in the database while mounted (another `scan`, a
beets/Picard/Lidarr sync, raw SQL) and the view refreshes automatically.

Run `musefs <command> --help` for the full flag list.

### Path templates

Paths come from a beets-style template (matched case-insensitively;
any tag key in the store works):

- `$field` / `${field}` — substitute a tag field (e.g. `$artist`, `$album`,
  `$title`, `$tracknumber`, `$date`, `$genre`).
- `${albumartist|artist}` — **fallback chain**: the first present field wins,
  before the `--default-fallback` value (default `Unknown`) is used.
- A missing field resolves in order: the field's value, then a **per-field
  fallback** from `--fallback FIELD=VALUE` (repeatable, e.g. `--fallback
  albumartist='Unknown Artist'`), then `--default-fallback`. Per-field
  fallbacks let one field default differently from the rest.
- `--skip-on-missing` — drop a track from the mount entirely when a **top-level**
  template field stays unresolved, instead of substituting `--default-fallback`.
  Per-field `--fallback` chains and `[ … ]` sections are unaffected (a field
  resolved via its fallback counts as present, and section fields stay optional).
  Handy when an external tool tags only some tracks, e.g.
  `--template '$!{beets_path}' --skip-on-missing` hides tracks beets left without
  a `beets_path` (such as deduplicated albums).
- `[ … ]` — **conditional section**: the bracketed text is emitted only when at
  least one field inside it is present. So `$album[ - CD $disc]` yields
  `Album - CD 2`, or just `Album` on a single-disc release. Write `$[` / `$]`
  for literal brackets.
- `$!{field}` — **path field**: the value's `/` are kept as directory
  separators (each segment sanitized; empty/`.`/`..` dropped). Lets an external
  tool precompute a whole relative path into one tag and mount it as
  `--template '$!{beets_path}'`.

Anything else is literal. Name collisions get a deterministic `(2)`, `(3)`, …
suffix. Every rendered component is capped at 255 bytes (NAME_MAX, truncated on
a UTF-8 boundary, extension preserved), and a plain field whose value is
exactly `.` or `..` is dropped rather than creating an unusable directory. The
default template is `$albumartist/$album/$title`.

Brackets and braces must be balanced: an unclosed `[` section or an
unterminated `${` / `$!{` field is rejected at mount time with an error naming
the problem, rather than silently folding the rest of the template into the
open construct. To check a template before committing to a mount, add
`--dry-run`: it validates the template, prints a sample of the paths the mount
would expose along with the total file and directory counts, then exits without
mounting.
