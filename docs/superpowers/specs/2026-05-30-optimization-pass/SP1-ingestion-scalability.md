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
equivalence is the **headline correctness guard** (see Testing & acceptance gate, item 1).

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

These probers are fed a **probe window** instead of the whole file:

- `prefix: &[u8]` — the first `prefix.len()` bytes of the file
  (`prefix.len() ≤ file_len`).
- `file_len: u64` — the true file size, from `fs::metadata`.

Contract:

- **`audio_length` is computed against `file_len`**, replacing today's
  `data.len() - audio_offset`. The audio bytes are not in `prefix`; their length
  is arithmetic, not a read.
- If parsing needs byte index `i` with `prefix.len() ≤ i < file_len`, the prober
  returns **`NeedMore { up_to: u64 }`** where `up_to` is the exact end of the
  structure it is mid-parse on. The current parsers already detect the truncation
  point (they return `Malformed` the instant a declared body length runs past the
  buffer); SP1 converts each such site from **"truncated → `Malformed`"** into
  **"truncated but the declared length is known → `NeedMore { up_to }`"** vs.
  **"truncated and length is unknowable → `Malformed`."** The field that yields
  `up_to` per format is named in the table below.
- The scan loop answers a `NeedMore` with **one widening read** up to `up_to`
  (capped at `file_len`) and retries the prober. Worst case for front-anchored
  formats is two bounded reads, never a full slurp — except WAV with metadata
  trailing a huge `data` chunk, whose `up_to` lands near `file_len` (≈ today's
  cost, no worse; see Out of scope).
- **MP3 ID3v1**: detected via a dedicated 128-byte **tail read**, separate from
  the prefix.

**Per-format reality (the work is not uniform).** Two formats already have a
front-only metadata split; two do not. The plan must budget accordingly:

| Format | Front-only metadata fn today | `read_tags`/`read_pictures` reach audio payload? | Field yielding `up_to` | Net work |
|---|---|---|---|---|
| **FLAC** | **Yes** — `read_metadata → FlacMeta { audio_offset, preserved }` (no `audio_length`); `locate_audio` is just that + `len - offset` (flac.rs:80/90) | No — metadata blocks (incl. PICTURE) are all `< audio_offset` | 24-bit METADATA_BLOCK length at the truncation point | **Small** — thread `file_len`, emit `NeedMore` from `parse_blocks` |
| **OGG** | **Yes** — `read_metadata(front) → OggHeader` is the front-only twin of `locate_audio` (ogg/mod.rs:202/208) | No — **all** art is in the header packets before `audio_offset`: Opus/Vorbis `METADATA_BLOCK_PICTURE` in the comment packet, OggFLAC type-6 packets (ogg/mod.rs:154–184) | end of header-packet reassembly (= `audio_offset`) | **Small** — emit `NeedMore` when header packets aren't fully present in `prefix` |
| **MP3** | **No** — `locate_audio(&[u8])` takes whole buffer (mp3.rs:26) | ID3v2 tag is front; ID3v1 is the 128-byte tail | ID3v2 synchsafe `tag_len` (fully known from the first 10 bytes, mp3.rs:36–40) | **Real** — add a front-only variant + tail read |
| **WAV** | **No** — `locate_audio(&[u8])` takes whole buffer (wav.rs:64) | LIST/INFO + `id3 ` chunks may sit **after** `data` | next-chunk offset from the RIFF chunk header (`walk_chunks`, wav.rs:35) | **Real** — front-only chunk walk; trailing-metadata case widens toward `file_len` (Out of scope) |

Note OGG art is **front-anchored by construction**, so a `WINDOW` covering the
header region captures it with no widening read; the only OGG widening trigger is
a header region (many/large embedded images) exceeding `WINDOW`.

`WINDOW` (initial prefix size) is a tunable constant; **default 1 MiB**. Embedded
front-cover art is routinely 0.5–3 MB, so for art-bearing FLAC/MP3 the widening
read is the **expected** path, not a rare one — but it is still bounded to
*metadata + the exact art extent*, never the audio payload, which is the whole
point. (The `custom`/real-library bench tier can later inform a `WINDOW` that
brackets a given library's p95 art size; the default is deliberately conservative
so a giant cover never wastes a slurp.)

### M4A: seek reader

M4A routes through `mp4::read_structure_from` (seek to `ftyp`/`moov` + `mdat`
header only). The moov-last audiobook case — the single largest slurp today — is
eliminated; `mdat` is never read.

### Per-file scan flow

1. `fs::metadata` → `file_len`, `mtime`. (`revalidate` first runs its main-thread
   pre-dispatch skip pass — Component 2 — so unchanged files never reach a worker
   and do no file read.)
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
- **Backpressure / memory budget**: the worker→writer channel is bounded so
  in-flight `Probed` values (which hold art `Vec<u8>` blobs) cannot balloon
  memory. The bound is **a byte budget over accumulated art**, and it is the
  *same* budget as the batch byte-ceiling (Component 3) — a `Probed` is held
  either in the channel or in the forming batch, so they are accounted as one
  pool, not two. Concretely: cap total in-flight art at **B<sub>bytes</sub>**
  (default **64 MiB**); peak scan RSS for art ≈ B<sub>bytes</sub> + the current
  batch. (`--jobs` only sets worker count, not the memory bound.)
- **Semantic (not literal) determinism across `--jobs`**: track rows are keyed by
  `backing_path` (upserted) and art is deduplicated by `art.sha256 UNIQUE`, so
  the *content* of the DB is independent of `--jobs`. But `art.id` is an
  insertion-order rowid (schema.rs:26), so with nondeterministic worker
  completion the same image can receive a different `art.id` (and hence
  `track_art.art_id`) run-to-run. The DB is therefore **semantically equivalent,
  not byte-identical at the id level**. The determinism test compares
  **normalized** state — tracks by `backing_path`, tags by `(key, value,
  ordinal)`, art by `sha256` + its per-track usage — **not** raw `art.id` values
  (see Testing & acceptance gate, item 3).
- **`revalidate` skip-unchanged must not put a DB read in the workers.** The
  size+mtime fast-skip (scan.rs:240) is a DB lookup, and workers are DB-free. So
  `revalidate` does a **pre-dispatch pass on the main thread**: load the existing
  `(backing_path → backing_size, backing_mtime)` set once (one query, the way
  prune already lists tracks), `stat` each candidate, and enqueue only the files
  whose size/mtime changed. Workers still only probe; unchanged files never reach
  them.
- The **directory walk stays single-threaded** (cheap relative to probing;
  parallelizing it is out of scope).
- `revalidate`'s whole-DB tail steps (prune missing tracks, `gc_orphan_art`) run
  after the pipeline drains, single-threaded on the writer connection.

## Component 3 — Transaction batching

- The writer accumulates files and wraps all their
  `upsert_track` + `replace_tags` + `upsert_art` + `set_track_art` calls in a
  **single transaction**, then commits — collapsing today's several
  commits-per-file into a handful per batch. This is the primary **fsync**
  reduction (one fsync per batch under `synchronous=NORMAL`, vs. several per file
  today), measured via `musefs-latencyfs` (Component 6).
- **Flush triggers (whichever trips first):** a **count** bound — default
  **B = 256** files, chosen to amortize commit/fsync overhead while keeping a
  failed-commit blast radius small — **or** the shared **art byte budget**
  B<sub>bytes</sub> (default **64 MiB**, the same pool as the channel
  backpressure in Component 2; measured as raw image-blob bytes, excluding
  framing). Both are tunable constants.
- **Trigger semantics are unaffected.** The `content_version`/`updated_at`
  triggers (schema.rs:44–74) fire **per row** and are **scoped to the owning
  `tracks` row** (`WHERE id = NEW.track_id`). Batching many tracks into one
  transaction does not cross-contaminate: each track's `replace_tags`
  (DELETE-all + INSERT-each, tags.rs:8–15) bumps only its own `content_version`,
  exactly as today.
- **Error semantics**: per-file read/probe failures are handled in the worker and
  **never enter a batch**, so batches need no partial-rollback logic. A DB error
  on commit is **fatal** (stops workers, propagates). A batch is a unit of
  durability only, not of correctness: a crash mid-scan loses at most the
  un-committed batch, which a re-scan re-ingests (additive); a crash mid-`revalidate`
  may also lose the destructive prune/GC tail, but re-running re-prunes
  idempotently.

## Component 4 — Bulk-write pragmas

A **scan-scoped bulk-write connection**, opened by `musefs-db` at the start of a
scan/revalidate and **closed at the end** — explicitly *distinct* from any
long-lived mount connection, so its pragmas never leak into the serve path. New
API surface SP1 must add (today `Db` is a single `Connection`, lib.rs:15, with no
bulk path):

- a constructor for the bulk connection (e.g. `Db::open_bulk`), and
- a **batch-writer handle** (e.g. `BulkWriter`) that wraps the per-track
  `upsert_track` / `replace_tags` / `upsert_art` / `set_track_art` calls in one
  transaction and exposes `flush`/`commit`.

Pragmas on that connection:

- `synchronous = NORMAL` — safe under WAL (survives app crash; only a power-loss
  on the very last commit is at risk, which a re-scan fixes since scan is
  re-runnable; see the revalidate caveat in Component 3).
- `cache_size` ≈ **-65536** (64 MiB).
- `temp_store = MEMORY`.
- **`journal_mode = WAL` retained.** WAL allows concurrent *readers* (a live
  mount keeps reading), but only **one writer**.
- `busy_timeout` retained (currently 5 s, lib.rs:43); `wal_checkpoint(TRUNCATE)`
  on close to bound WAL growth.

**Exclusive-write assumption (stated, not assumed silently).** `musefs scan` /
`revalidate` assumes it is the **sole writer** for its duration. WAL's single-writer
rule means a long-running bulk transaction holds the write lock; a *second*
concurrent writer (e.g. a beets/picard sync to the same DB) would hit
`SQLITE_BUSY` once the 5 s `busy_timeout` is exceeded by a slow batch. This is
acceptable because scan is an operator-initiated maintenance pass, not a
steady-state mount activity; the roadmap's out-of-band writers are expected to
run a scan, not concurrently with one. A concurrent live mount only *reads* and
is unaffected. (If concurrent external writes during scan ever become a real
requirement, smaller batches / a shorter lock-hold would be the lever — out of
scope here.)

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
   `WINDOW` to force the `NeedMore`/widen path — yields a `Probed` **structurally
   equal** (all fields; tags and pictures **order-preserved**, since ordinals
   drive template rendering and synthesis) to the legacy full-buffer probe.
   Precisely, for each file: `bounded.audio_offset == legacy.audio_offset`;
   `bounded.audio_length == file_len - bounded.audio_offset == legacy.audio_length`;
   `bounded.tags == legacy.tags` as an ordered list of `(key, value)`; and
   `bounded.pictures == legacy.pictures` as an ordered list (bytes, mime,
   picture_type, description). ("Structurally equal," not "byte-identical" — the
   latter is the *serve-path* guarantee; this asserts the parsed values match.)
   This is the core guard that the served output cannot drift.
2. **`NeedMore`/widen units.** Tiny window → correct `up_to`, exactly one
   widening read, embedded art beyond the window still captured intact. Includes
   the OGG case (incomplete header-packet reassembly in `prefix` → `NeedMore`) and
   the WAV trailing-metadata case (`up_to` near `file_len`).
3. **Parallel determinism (normalized).** `--jobs 1` vs `--jobs N` over the same
   corpus → **semantically equivalent** DB state, compared on **normalized** form:
   tracks by `backing_path`; tags by `(key, value, ordinal)`; art by `sha256` plus
   its per-track usage `(picture_type, description, ordinal)`. Raw `art.id` /
   `track_art.art_id` values are **excluded** from the comparison (they are
   insertion-order rowids and legitimately differ across runs).
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
- **APEv2 tags** — not read by the current MP3 prober (only ID3v1 + ID3v2) and not
  emitted by synthesis; SP1 does not change that. The 128-byte tail read is
  ID3v1-only.
- **Parallelizing the directory walk** — cheap relative to probing.
- **Any serve/read-path change** — SP1 is ingestion-only.
- **New formats.**
- No storage-bound validation is deferred: SP0b's `musefs-latencyfs` and a real
  `bandwidth`-tier mount both run on the current box.

## Implementation sequencing (for the plan)

This is one SP, but it has a natural two-stage order the plan should follow so the
riskiest change is verified before the concurrency machinery is layered on:

1. **Stage A — bounded reads (Component 1), still single-threaded.** Land the
   probe-window / `file_len` / `NeedMore` contract and the M4A seek route, gated
   green by the **equivalence property (Testing item 1)** before anything else. This is the
   only change that touches the byte-identical parsing surface.
2. **Stage B — pipeline (Components 2–4).** Add the parallel-probe/serial-writer
   pool, the `BulkWriter` batching, the scan-scoped bulk connection, the
   `revalidate` pre-dispatch skip pass, and resilience counting — on top of an
   already-verified Stage A.

Stages may ship as one or two PRs at the implementer's discretion; the gating
order (A's equivalence property green before B) is the requirement.
