# SP1 — Ingestion scalability — design

*Date: 2026-05-30 · Part of the [2026-05-30 optimization pass](./README.md)*

## Goal

Make `scan` / `revalidate` fast and bounded-memory over large libraries
**without changing what is written to the DB**. Today the scan loop slurps every
backing file whole (`std::fs::read` at `scan.rs:207` and `:249`), probes
single-threaded, and commits per file. SP1 delivers four things:

1. **Bounded probing reads** — stop reading whole files; read only the metadata
   region (plus the exact embedded-art extent), never the audio payload.
2. **Parallel probing** — a bounded worker pool probes concurrently, default-on,
   with a `--jobs` knob; a single writer thread keeps SQLite single-writer-safe.
3. **Transaction batching** — group many files per transaction instead of
   committing per file.
4. **Bulk-write pragma tuning** — a dedicated bulk connection trades a sliver of
   crash-durability (re-runnable anyway) for throughput.

## Cardinal invariant (preserved structurally)

SP1 changes only *how metadata is extracted at scan time*. It writes the same
`audio_offset` / `audio_length` / `tags` / `art` rows, and **never touches the
serve/read path**. So the byte-identical-audio guarantee holds by construction —
provided bounded probing produces results identical to full-file probing. That
equivalence is the **headline correctness guard** (see Testing §8.1).

## Reuse (existing foundation)

- **`mp4::read_structure_from<R: Read + Seek>`** (`mp4.rs:244`, built in the
  2026-05-26 Phase 6) already seeks to the `moov` atom wherever it lives and
  never reads `mdat`. SP1 routes M4A scanning through it instead of the
  slice-based path — the one format whose metadata can legitimately sit at the
  end of a multi-hundred-MB file.
- **`musefs-core/src/metrics.rs`** — feature-gated atomic serve-path counters
  (`on_open`/`on_stat`/`on_pread`/`on_art_chunk`) with `Snapshot` +
  `snapshot()`/`reset()`. SP1 *adds* scan-path counters here (none exist yet).
- **`musefs-latencyfs`** (SP0b) — passthrough FUSE hosting corpus **and** DB
  under the mount, injecting per-op latency (`ssd`/`hdd`/`nfs` profiles) and
  counting `fsync`/`fsyncdir` ops (`LatencyFs::fsyncs()`). SP1 measures scan
  fsyncs and latency-sensitivity through it — no new in-process fsync counter.
- **`musefs-core/tests/bench_ingest.rs`** + the SP0a corpus generator and
  `RunReport` — extended, not replaced, to report the new scan signals.
- **`db_pool.rs`** worker-pool shape is the reference pattern for the probe pool
  (though SP1's pool is write-free — only the single writer thread touches the
  DB).

## Component 1 — Bounded probing reads (Hybrid)

Replace the whole-file slurp with a bounded contract. Metadata location varies by
format, and only M4A can legitimately place it at the file's end, so the strategy
splits accordingly.

### Front-anchored formats: FLAC, MP3, OGG, WAV

The prober entry points (`locate_audio`, `read_vorbis_comments` / `read_tags`,
`read_pictures`, `read_header`) change signature to accept a **probe window**:

- `prefix: &[u8]` — the first `prefix.len()` bytes of the file
  (`prefix.len() ≤ file_len`).
- `file_len: u64` — the true file size, from `fs::metadata`.

Contract:

- **`audio_length` is computed against `file_len`**, replacing today's
  `data.len() - audio_offset`. The audio bytes are not in `prefix`; their length
  is arithmetic, not a read.
- If parsing needs byte index `i` with `prefix.len() ≤ i < file_len`, the prober
  returns **`NeedMore { up_to: u64 }`** carrying the exact high-water byte it
  needs. Block/frame/chunk *headers* are tiny and front-anchored, so a small
  prefix lets a prober learn the exact extent of bodies (notably embedded art)
  and request precisely that range.
- The scan loop answers a `NeedMore` with **one widening read** up to `up_to`
  (capped at `file_len`) and retries the prober. Worst case for front-anchored
  formats is two bounded reads, never a full slurp — except WAV with metadata
  trailing a huge `data` chunk, whose `up_to` lands near `file_len` (≈ today's
  cost, no worse; see §9).
- **MP3 ID3v1**: detected via a dedicated 128-byte **tail read**, separate from
  the prefix.

`WINDOW` (initial prefix size) is a tunable constant; default **1 MiB**. Typical
metadata + cover art fits inside it, so the widening read is rare.

### M4A: seek reader

M4A routes through `mp4::read_structure_from` (seek to `ftyp`/`moov` + `mdat`
header only). The moov-last audiobook case — the single largest slurp today — is
eliminated; `mdat` is never read.

### Per-file scan flow

1. `fs::metadata` → `file_len`, `mtime`. (`revalidate`: unchanged size+mtime →
   skip with no file read, as today.)
2. Read `prefix = min(file_len, WINDOW)` bytes.
3. Dispatch by extension:
   - FLAC/MP3/OGG/WAV → probers with `(prefix, file_len)`; MP3 also issues the
     128-byte tail read when the ID3v1 region is outside the prefix.
   - M4A → `read_structure_from` over an opened `File`.
4. On `NeedMore { up_to }` from a front-anchored prober → one widening read to
   `up_to`, retry. (`up_to == file_len` is allowed; it is the rare full-read
   case.)
5. Produce `Probed`; hand to the writer.

The legacy slice-based prober bodies are largely reused — the change is the
window/`file_len`/`NeedMore` plumbing at their boundaries, keeping the hardened
fuzz + byte-identical parsing surface intact.

## Component 2 — Parallel probing (default-on, knob)

```
collect_audio (main thread)
        │  paths
        ▼
   [bounded work queue]
        │
   N probe workers  ── pure parse + bounded file reads, no DB ──┐
        │  Probed / Skipped / Failed                            │
        ▼                                                       │
   [bounded results channel]  ◄── backpressure caps in-flight art blobs
        │
   1 writer thread  ── owns the bulk-write connection, batches txns
        ▼
     SQLite
```

- **Workers**: default count = `std::thread::available_parallelism()`; `--jobs N`
  overrides; **`--jobs 1` collapses to a fully sequential scan** (canonical
  measurement baseline + escape hatch on constrained boxes). Workers do pure
  parsing and bounded file reads — they never touch the DB.
- **Single writer thread** owns the one write connection, so SQLite stays
  single-writer-safe with zero write contention.
- **Bounded channel** between workers and writer provides backpressure, so
  in-flight `Probed` values (which hold art blobs) cannot balloon memory —
  protecting the bounded-memory goal.
- **Order-independence**: upserts are keyed by `backing_path`, so final DB state
  is identical regardless of `--jobs`. `--jobs 1` and `--jobs N` converge to the
  same rows — keeping tests deterministic.
- The **directory walk stays single-threaded** (cheap relative to probing;
  parallelizing it is out of scope).
- `revalidate` uses the same worker/writer shape; its whole-DB tail steps (prune
  missing tracks, `gc_orphan_art`) run after, single-threaded on the writer
  connection.

## Component 3 — Transaction batching

- The writer accumulates **B files** and wraps all their
  `upsert_track` + `replace_tags` + `upsert_art` + `set_track_art` calls in a
  **single transaction**, then commits — collapsing today's several
  commits-per-file into a handful per batch.
- **Batch sizing**: count-based default (**B = 256**) with a **byte ceiling** on
  accumulated art so a batch can neither hold too much memory nor build an
  oversized transaction; whichever bound trips first flushes the batch.
- **Error semantics**: per-file read/probe failures are handled in the worker and
  **never enter a batch**, so batches need no partial-rollback logic. A DB error
  on commit is **fatal** (stops workers, propagates).

## Component 4 — Bulk-write pragmas

A dedicated **bulk-write connection** (owned by `musefs-db`, the pragma-policy
owner — e.g. a `Db::open_bulk` / bulk-configured connection):

- `synchronous = NORMAL` — safe under WAL (survives app crash; only a power-loss
  on the very last commit is at risk, which a re-scan fixes since scan is
  re-runnable).
- `cache_size` ≈ **-65536** (64 MiB).
- `temp_store = MEMORY`.
- **`journal_mode = WAL` retained** — a concurrent live mount keeps reading.
- `busy_timeout` retained; optional `wal_checkpoint(TRUNCATE)` at scan end to
  bound WAL growth.

## Component 5 — Resilience

Per-file I/O and probe errors are **logged and counted**, and the scan
**continues**; only writer/DB errors are fatal. (Today a single `?` aborts the
whole scan.) `ScanStats` / `RevalidateStats` gain a **`failed`** field alongside
`scanned` / `skipped`. `probe`-returns-`None` continues to count as `skipped`.

## Component 6 — Measurement

- **New in-process scan counters** in `metrics.rs` (behind the existing `metrics`
  feature): `scan_opens`, `scan_preads`, `scan_bytes_read` (+ a batch/commit
  count), wired through the prefix read, any widening read, the MP3 tail read,
  the M4A seek reads, and the writer. These prove bounded reads directly:
  `scan_bytes_read` per file drops from full-file size to ≈ `WINDOW` + exact art
  extent.
- **fsyncs** are measured externally by running a scan against a
  `musefs-latencyfs`-mounted corpus+DB and reading `LatencyFs::fsyncs()` — the
  direct signal for transaction batching's win. No in-process fsync counter.
- `bench_ingest` is extended to report `scan_bytes_read` per format (and make
  `opens`/`preads` meaningful) and to take a `jobs` dimension. Before/after
  numbers land in the umbrella results log.

## Testing & acceptance gate

1. **Equivalence property (headline).** For every format fixture **and**
   proptest-generated files, bounded probing — run with a deliberately tiny
   `WINDOW` to force the `NeedMore`/widen path — yields a `Probed`
   (`audio_offset`, `audio_length`, `tags`, `pictures`) **byte-identical** to the
   legacy full-buffer probe. This is the core guard that the served output cannot
   drift.
2. **`NeedMore`/widen units.** Tiny window → correct `up_to`, exactly one
   widening read, embedded art beyond the window still captured intact.
3. **Parallel determinism.** `--jobs 1` vs `--jobs N` over the same corpus →
   identical DB state (rows + tags + art).
4. **Resilience.** A corrupt/unreadable file in the corpus → scan completes, that
   file counted `failed`, all others ingested.
5. **Batching.** A forced DB error mid-batch is fatal and surfaces.
6. **Regression gates.** All crate tests + format fuzz seeds + interop + the
   `#[ignore]` e2e mount suite stay green; byte-identical audio holds; the
   Criterion `ci` `sequential_read` median stays within the **<10%** run-over-run
   rule (SP1 barely touches the read path, but the gate is enforced).
7. **Benches / evidence.** `bench_ingest` shows `scan_bytes_read` and `opens`
   drop sharply on large-payload files; fsyncs (via `musefs-latencyfs`) drop with
   batching. Validation spans `ci` + `large-compute` (tempfs — compute/RSS), the
   **`bandwidth` tier on a real mount** (1,000 × ~30 MiB payloads, where bounded
   reads pay off most), and `musefs-latencyfs` `ssd`/`hdd`/`nfs` profiles
   (fsync + latency-sensitivity) — all runnable on the current box (6 cores,
   `/dev/fuse`). Numbers recorded in the results log.

## Out of scope (YAGNI)

- **Seek-based RIFF chunk-walk for WAV** — WAV with metadata trailing a huge
  `data` chunk widens toward `file_len` (no worse than today). A seek-based walk
  is a possible follow-up.
- **Parallelizing the directory walk** — cheap relative to probing.
- **Any serve/read-path change** — SP1 is ingestion-only.
- **New formats.**
- No storage-bound validation is deferred: SP0b's `musefs-latencyfs` and a real
  `bandwidth`-tier mount both run on the current box.
