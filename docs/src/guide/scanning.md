# Scanning

`musefs --version` (or `-V`) prints the build version; `--help` on the root or
any subcommand lists its flags.

## Scan

```bash
musefs scan /path/to/music --db library.db             # additive ingest
musefs scan /path/to/music --db library.db --force     # reseed existing rows
```

`scan` probes each audio file (FLAC, MP3, M4A/M4B, Ogg, WAV), recording its
audio byte range, tags, and embedded art in the store. Bare `scan` is
additive: it leaves already tracked rows alone, while `--force` re-seeds
existing rows from disk. (Refreshing already-tracked files and pruning gone
ones is the job of `musefs revalidate` — see
[Maintenance](maintenance.md#refreshing-the-store-musefs-revalidate).) It takes
one or more files or directories, and `--jobs N` controls probe parallelism. `--follow-symlinks` walks symlinked
files and directories (off by default, so symlinks are logged and skipped).
`--quiet` (`-q`) suppresses the per-target summary for scripting; scan
failures still surface on stderr (raise detail with `-v`/`-vv`, or
`RUST_LOG=info`).

`scan` (with or without `--force`) shows a live progress indicator: on an
interactive terminal, a discovery spinner followed by a determinate bar
(position, percent, ETA, current file); on a non-interactive stderr (piped or
logged), throttled `processed N/M (P%)` lines. `--quiet` (`-q`) suppresses the
progress indicator and the per-target summary. Each summary line ends with the
elapsed time. Anything logged during the scan (skip warnings, per-file
failures) is printed above the bar, which is lifted out of the way and redrawn
underneath, so warnings stay readable and scroll back intact.

The per-target summary reads
`scanned <target>: N file(s), Z already present, skipped X, failed Y in <elapsed>`,
where `N` counts the files stored or refreshed.
`already present` counts files bare `scan` skipped because they were already
tracked. `skipped`
counts every file that isn't a supported audio format — cover art, `.cue` /
`.log` / `.nfo` sidecars, and anything else non-audio — so a large `skipped`
number (hundreds or thousands on a big library) is expected, not an error.
A per-extension breakdown of the skip count is logged at end of scan at
`info` (e.g. `skipped 42: jpg=20, cue=10, log=8, <none>=4`, so it needs `-v` or
`RUST_LOG=info`), letting you tell expected sidecars from anything genuinely
unexpected. `failed` is the one to watch: those are audio files musefs
recognised by extension but could not parse, or could not store. Its own
breakdown by reason is logged at end of scan too —
`failed 38: unparseable=30, io=5, oversize=2, rejected=1` — at `warn`, so it is
visible without `-v`; a further `walk errors N: unreadable=9, symlink=3` line
accounts for directories and entries the walk itself could not read (those are
counted in neither `skipped` nor `failed`, since no file was ever queued for
them).

A `rejected` bucket in that breakdown means the store refused a file's rows on
a constraint — the tag, art or track values it parsed were not something the
schema accepts. Each one is logged with its path and the constraint text, and
the rest of the library scans normally; nothing partial is stored for a rejected
file, so the mount never shows a track quietly missing its tags. These are worth
reporting: unlike `oversize`, which names a documented limit, a `rejected` file
is a shape musefs did not anticipate.

Two more buckets are new in 2.0.0. `unsupported` counts files that parsed but
hold a shape musefs refuses to serve: a chained Ogg, several streams
concatenated end to end ([Ogg](../formats/ogg.md#one-bitstream-per-file)). A row
1.3.0 stored for one is removed by `musefs revalidate --prune`.
`checksum-failed` counts files whose checksum, at the tier `--checksum` asks
for, could not be computed; such a file is failed rather than stored one tier
lower.

Per-file skip messages are capped at ten per reason per scan; the rest drop to
`debug` (`-vv` / `RUST_LOG=debug`) so an unreadable subtree or a share that
vanished mid-scan cannot emit one line per file. The end-of-scan breakdowns
carry the full counts either way. Format dispatch is by **extension only** —
there is no content sniffing and no fallback to another parser, so a file
whose contents don't match its extension (e.g. a FLAC named `.mp3`) is handed
to the wrong parser, fails, and is counted here rather than retried. Renaming
files across formats makes them vanish from the mount; fix the extension and
rescan.

Non-standard containers are tolerated where the audio is still recoverable: a
FLAC that carries one or more ID3 tags in front of the `fLaC` marker parses,
and any tags or cover art in that ID3 header are ingested as a fallback for
what the FLAC itself does not carry — see
[Leading ID3 tags](../formats/flac.md#leading-id3-tags).

If any file fails (`failed Y` with `Y > 0`), `scan` exits **2** even though the
batch otherwise completes and the parseable files are ingested — so a pipeline
like `musefs scan … && musefs mount …` stops on a partial or total ingest
failure rather than mounting an incomplete library. A successful scan exits `0`;
a hard error (a missing target, an unreadable DB) still exits `1`. The exit code
is the only machine-detectable signal; per-file failures otherwise surface only
on stderr. The full exit-code contract, and how to raise log detail on those
failures, are in
[Logging & troubleshooting](troubleshooting.md#exit-codes).

### Content checksums and move re-identification

`--checksum=none|fingerprint|full` (env `MUSEFS_CHECKSUM`, default
`fingerprint`) controls what content checksums `scan` computes and stores.

- **`none`** — no checksums (legacy behavior).
- **`fingerprint`** — compute a cheap fingerprint for each file, derived from
  the probe's parsed output (tags, audio bounds, embedded art) plus three
  bounded windows of audio sampled at the start, midpoint and end of the audio
  region. This is the default: it rides the existing probe, adding at most
  24 KiB of positioned reads per file and no whole-file pass, and it is
  sufficient for routine move detection. The audio windows are what make it so
  for every format — without them, two different MP3, M4A, Ogg or WAV files
  with the same tags, the same art and an equal audio length share one
  fingerprint, and a move can retarget the wrong row. It samples the audio
  rather than hashing all of it, so it remains a heuristic: two files that agree
  on every sampled window and differ only between them still collide.
- **`full`** — fingerprint plus an eager full-file SHA-256. Use this when you
  want collision-proof retargeting or a forensic content identity for every
  file. A file this tier cannot hash is **failed**, not ingested one tier
  lower: it is counted in `failed` (under `checksum-failed` in the end-of-scan
  breakdown) and so reaches the exit-`2` partial-failure signal.

`--match` (env `MUSEFS_MATCH`) governs how a fingerprint match is confirmed
before a moved file is retargeted:

- **`auto`** (default) — escalate when there is something to escalate to:
  full-hash the new file when the matched candidate already has a
  `content_hash`, and trust the fingerprint alone when it does not.
- **`fast`** — a fingerprint match is always sufficient; never reads the full
  file, even when a stored `content_hash` exists.
- **`strict`** — require a full-hash match; if the matched candidate has no
  stored `content_hash`, refuse the retarget and insert a fresh row instead.

**Upgrading from musefs 1.3.0 or earlier.** Every command other than
`migrate` refuses a 1.3.0 store until `musefs migrate --db library.db` upgrades it (see
[Maintenance](maintenance.md#upgrading-the-store-musefs-migrate)). That upgrade
clears every stored fingerprint, because the value now includes sampled audio
and the old ones were computed without it. The next `revalidate` recomputes
them with no flag needed, since it re-probes a row missing the checksum its tier
asks for; a plain `scan` does not, because it leaves already-tracked rows alone.
Until then those rows cannot be move-recovered, exactly as an unfingerprinted
row never could, so run one pass before moving files around, and `mount`,
`scan` and `revalidate` each print a warning with the number still waiting.
The upgrade clears every stored `content_hash` too. Before #689 was fixed, a
fingerprint-tier rescan of a rewritten file could keep the old bytes' hash, so
none is carried across. Run `musefs revalidate --checksum=full` to recompute
them; until then `--match=auto` confirms a move by fingerprint alone, and
`--match=strict` refuses to retarget.

**Move re-identification workflow.** After moving or reorganizing your backing
library, run a normal `musefs scan` on the new locations. For each file not
already in the store, the scanner looks up rows whose fingerprint matches and
whose old path is gone, and retargets the unique match in place — its `id`,
tags, and art are preserved. Move recovery only applies to rows that were
fingerprinted before the move. Rows scanned under `--checksum=none` have no
fingerprint, and once their files move nothing can give them one: `revalidate`
ignores files at new paths, and the old ones are gone. Run a fingerprint-tier
`musefs revalidate` over them *before* moving the files, while each row still
points at its file.

A `content_hash` only ever describes the file a row currently points at. A pass
that computes no full hash — a `fingerprint`-tier revalidate of a rewritten file,
or a `--match=fast` retarget that confirms nothing — clears the column rather than
leaving the previous bytes' hash standing. A `revalidate --checksum=full`
restores it. A pass over a file that has not changed keeps the hash it already
has, so a cheap pass never undoes an expensive one.
Run `scan` after a move and ideally **before** any `revalidate` — `revalidate`
only refreshes already tracked rows, so a moved file must be re-seeded before
the maintenance pass can see it. Use `revalidate --prune` only when you are
ready to drop missing rows.
