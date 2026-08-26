# Logging & troubleshooting

Every musefs crate reports through the `log` facade; the `musefs` binary
installs the sink and writes it to **stderr**. Nothing is logged to the mount
or to the store, so the log is wherever that process's stderr goes — your
terminal, the journal, or the container log.

## Raising log detail

The default floor is `warn`: enough that a failure is never silent, quiet
enough to leave running. `-v` raises it, and repeats stack:

| Flag | Level | What it adds |
| ---- | ----- | ------------ |
| *(none)* | `warn` | Failures only: serve-path errors, skipped/failed files, deprecations. |
| `-v` | `info` | What the mount actually did — the mount summary and the passthrough decision. |
| `-vv` | `debug` | Per-op detail, including everything the warn limiter suppressed. |
| `-vvv` | `trace` | No musefs call site logs at trace today; it only opens up dependencies. |

`-v` / `--verbose` is global — `musefs -v mount …` and `musefs mount … -v` are
equivalent.

**`RUST_LOG` wins.** The `-v` count only supplies the *default* filter, so an
explicit `RUST_LOG` in the environment takes precedence and `-v` then has
nothing to do. That is the lever to reach for when you cannot edit the command
line (a systemd unit, a container `ExecStart`), and it is also the only way to
filter per crate:

```bash
RUST_LOG=info musefs mount /mnt/music --db library.db
RUST_LOG=warn,musefs_fuse=debug musefs mount /mnt/music --db library.db
```

Targets are crate names with underscores: `musefs`, `musefs_cli`,
`musefs_core`, `musefs_db`, `musefs_fuse`, `musefs_format`. There is no
`MUSEFS_*` variable for verbosity — unlike the mount and scan flags, this one
is `RUST_LOG` or `-v` only.

Two kinds of output ignore the log level entirely: `scan`/`revalidate` progress
and per-target summaries (suppressed with `--quiet` / `-q`, see
[Scanning](scanning.md#scan)), and a handful of CLI diagnostics printed
directly to stderr — `warning: mountpoint … is not empty`, the `--file-mode` /
`--dir-mode` write-bit warnings, and the final `musefs: <error>` on a hard
failure.

## What each level buys

### `warn` — the default

- Serve-path failures behind the mount: `read(…) failed: …`,
  `lookup(…) failed: …`, and `read(…) rejected: EAGAIN` load-shedding. These
  go through the [warn limiter](#the-serve-path-warn-limiter).
- Scan-time refusals: `skipping <path>: <reason>`, `skipping <path>: no
  parseable audio metadata`, and the per-extension breakdown of the skip count
  (`skipped 42: jpg=20, cue=10, …`).
- Degraded-but-correct fallbacks: `incremental tree mutation failed …; falling
  back to full rebuild`, `poll_refresh failed`, `inval_inode(…) failed`.
- Tag keys dropped during Vorbis synthesis (`track N: dropping tag key … (not a
  valid field name)`) — these are per-synthesis and are *not* rate-limited.
- `scan --revalidate` deprecation.
- `error` sits above it, reserved for a caught panic in a synthesis or scan
  worker, a scan aborted mid-ingest, and recovered lock poisoning.

### `-v` (`info`) — "why is my mount doing this"

This is the level worth knowing about. The lines that explain a mount's
behaviour are all `info`, so by default they are invisible:

- The pre-mount line naming the store, mountpoint, and template, then the
  post-mount summary once the session is actually serving:

  ```text
  musefs mounted at /mnt/music: 41230 files in 3812 directories (Synthesis)
  ```

  An empty or wrong `--db` is otherwise indistinguishable from a correctly
  serving one — the mount succeeds either way; the counts are the tell.

- The passthrough decision. In `structure-only` mode on Linux 6.9+ musefs
  registers the backing fd with the kernel so reads bypass the daemon; when
  that is not available it says so once and serves reads itself:

  ```text
  FUSE passthrough unavailable; serving reads through the daemon: <error>
  StructureOnly mount without CAP_SYS_ADMIN: kernel passthrough unavailable; reads will be served by the daemon
  StructureOnly mount: kernel passthrough is Linux-only; reads will be served by the daemon
  ```

  This is the single best explanation for "why is my `structure-only` mount
  slower than I expected".

- `changelog gap; falling back to full refresh` — the mount slept past more
  external edits than the changelog ring retains, so it rebuilt the tree
  wholesale instead of applying a diff. Correct, just more expensive.

If a mount is behaving oddly and you only reach for one thing, reach for `-v`.

### `-vv` (`debug`)

- Everything the warn limiter dropped, tagged `(over warn budget)`.
- Routine tree-shape misses that never warn: `lookup(…) failed: no such inode:
  …` and friends (`ENOENT` / `EISDIR` / `ENOTDIR`), which a kernel path probe
  and a stale inode after a refresh generate constantly.
- `opendir(…) over the 1024-handle cap: serving it statelessly` — the degraded
  readdir path a parallel walk can push a mount onto. The operator-facing
  signal for this is the `musefs_dir_handle_rejections_total` counter in the
  [metrics surface](tuning.md#metrics); the log line is per-occurrence.
- Scan-walk detail: duplicate backing targets, symlink handling.

## The serve-path warn limiter

Serve-path failure warns are rate-limited process-wide: **10 warns per 30 s
window**. Over budget, a message is downgraded to `debug` with `(over warn
budget)` appended and counted; the first warn of the next window carries the
tally:

```text
read(1042) failed: backing file changed since scan: /music/a.flac (317 similar serve-path warnings suppressed in the last 30s)
```

Read that line carefully — the suppressed failures are *not* necessarily the
same failure as the one printed. The count is a volume signal, not a
multiplicity of that message.

The limiter exists because the failure mode it guards is per-file: a library
enumeration over a corpus whose backing files have moved would otherwise emit
one warn per file — hundreds of thousands of lines, tens of MB of log, for a
single walk. It covers the `reply_errno` warn arm (the failures behind
`lookup`, `getattr`, `open`, `read`, `readdir`) and the read load-shedding
`EAGAIN` line. It does not cover scan-time warns or synthesis warns.

To see every suppressed failure, run at `-vv` (or `RUST_LOG=debug`): nothing is
discarded, only downgraded.

## Reading the logs under systemd

The [user units](../integrations/systemd.md) send stderr to the journal:

```bash
journalctl --user -u musefs -f          # follow the mount
journalctl --user -u musefs-scan -e     # the last of a scan run
```

`ExecStart` in the shipped unit is a bare `musefs mount` driven by `MUSEFS_*`
environment variables, so the least invasive way to raise detail is the
environment rather than the command line — add `RUST_LOG=info` to
`~/.config/musefs/musefs.conf`, or set it in a drop-in
(`systemctl --user edit musefs`):

```ini
[Service]
Environment=RUST_LOG=info
```

Then `systemctl --user restart musefs`. Adding `-v` works too, but it means
editing `ExecStart` itself.

## Reading the logs in a container

stderr is the container's log stream, so nothing special is needed to capture
it:

```bash
docker logs -f musefs        # podman logs -f musefs
```

Raise detail with `-e RUST_LOG=info` on `docker run` / `podman run`, or by
appending `-v` to the command after the image name. Note that the most common
container failure — a missing `--device /dev/fuse` or `--cap-add SYS_ADMIN` —
is not a log line at all: it is a hard error, `musefs: mounting at /mnt/musefs:
…`, and the container exits 1. See
[Running in containers](containers.md#required-flags).

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0` | Success. For `mount`, a clean unmount. |
| `2` | `scan` / `revalidate` completed, but at least one file failed to ingest (`failed Y` with `Y > 0`). The parseable files *are* in the store. |
| `1` | Hard error — a missing target, an unreadable or absent store, a mount that could not be established. The message is printed as `musefs: <error>`. |

Exit `2` is what makes a partial ingest machine-detectable, so
`musefs scan … && musefs mount …` stops instead of mounting an incomplete
library — the per-file failures otherwise surface only on stderr. It overlaps
clap's usage-error code, but a usage error fails before any work starts, so the
two are never ambiguous in a pipeline that got as far as running. See
[Scanning](scanning.md#scan) for what `failed` counts.

## When the log is not the answer

- A file that will not open or errors on read is usually a backing file that
  changed since its last scan — see the [FAQ](faq.md) and run
  `musefs revalidate`.
- For steady-state behaviour (handle counts, cache hit rates, read errors,
  degraded-readdir rejections) the counters in
  [`.musefs-metrics/`](tuning.md#metrics) are the surface to watch; the log
  reports events, not rates.
