# Scanning

`musefs --version` (or `-V`) prints the build version; `--help` on the root or
any subcommand lists its flags.

### Scan

```bash
musefs scan /path/to/music --db library.db            # ingest (dirs recurse)
musefs scan /path/to/music --db library.db --revalidate
```

`scan` probes each audio file (FLAC, MP3, M4A/M4B, Ogg, WAV), recording its
audio byte range, tags, and embedded art in the store. It takes one or more
files or directories, and `--jobs N` controls probe parallelism.
`--follow-symlinks` walks symlinked files and directories (off by default, so
symlinks are logged and skipped). `--quiet`
(`-q`) suppresses the per-target summary for scripting; scan failures still
surface on stderr (raise detail with `RUST_LOG=info`).

`scan` and `scan --revalidate` show a live progress indicator: on an interactive
terminal, a discovery spinner followed by a determinate bar (position, percent,
ETA, current file); on a non-interactive stderr (piped or logged), throttled
`ingested N/M (P%)` lines. `--quiet` (`-q`) suppresses the progress indicator
and the per-target summary. Each summary line ends with the elapsed time.

The per-target summary reads `scanned N: … skipped X, failed Y`. `skipped`
counts every file that isn't a supported audio format — cover art, `.cue` /
`.log` / `.nfo` sidecars, and anything else non-audio — so a large `skipped`
number (hundreds or thousands on a big library) is expected, not an error.
A per-extension breakdown of the skip count is logged at end of scan (e.g.
`skipped 42: jpg=20, cue=10, log=8, <none>=4`), so you can tell expected
sidecars from anything genuinely unexpected. `failed` is the one
to watch: those are audio files musefs recognised by extension but could not
parse. Format dispatch is by **extension only** —
there is no content sniffing and no fallback to another parser, so a file
whose contents don't match its extension (e.g. a FLAC named `.mp3`) is handed
to the wrong parser, fails, and is counted here rather than retried. Renaming
files across formats makes them vanish from the mount; fix the extension and
rescan.

`--revalidate` is the maintenance pass: it skips unchanged files —
**preserving any tag edits you made in the store** — prunes tracks whose
backing file is gone, and garbage-collects orphaned art.

#### Content checksums and move re-identification

`--checksum=none|fingerprint|full` (env `MUSEFS_CHECKSUM`, default
`fingerprint`) controls what content checksums `scan` computes and stores.

- **`none`** — no checksums (legacy behavior).
- **`fingerprint`** — compute a cheap fingerprint for each file, derived from
  the probe's parsed output (tags, audio bounds, embedded art). This is the
  default: it rides the existing probe at essentially no extra I/O cost and
  is sufficient for routine move detection.
- **`full`** — fingerprint plus an eager full-file SHA-256. Use this when you
  want collision-proof retargeting or a forensic content identity for every
  file.

Two flags govern how a fingerprint match is confirmed before retargeting a
moved file:

- **`--fast`** (env `MUSEFS_FAST`) — fingerprint match is always sufficient;
  never reads the full file even when a stored `content_hash` exists.
- **`--strict`** (env `MUSEFS_STRICT`) — require a full-hash match; if the
  matched candidate has no stored `content_hash`, refuse the retarget and
  insert a fresh row instead. The default (neither flag) auto-escalates:
  full-hash the new file when the candidate already has a `content_hash`,
  and trust the fingerprint alone when it does not.

`--fast` and `--strict` are mutually exclusive.

**Move re-identification workflow.** After moving or reorganizing your backing
library, run a normal `musefs scan` on the new locations. For each file not
already in the store, the scanner looks up rows whose fingerprint matches and
whose old path is gone, and retargets the unique match in place — its `id`,
tags, and art are preserved. Move recovery only applies to rows that were
fingerprinted before the move (rows scanned under `--checksum=none` have no
fingerprint and cannot be retargeted until a later fingerprint-tier pass).
Run `scan` after a move and ideally **before** any `revalidate` — `revalidate`
still prunes tracks whose backing file is gone, so it will remove un-retargeted
rows if run first.
