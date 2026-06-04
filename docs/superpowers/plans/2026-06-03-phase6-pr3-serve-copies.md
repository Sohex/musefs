# Phase 6 PR 3 — Serve-path copies (#70) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the two serve-path copies we control: DB-blob chunks (art/binary-tag) pass through an intermediate `Vec` before landing in the output buffer, and every FUSE read allocates a fresh output `Vec`.

**Architecture:** Half A: `Db::read_art_chunk_into` / `read_binary_tag_chunk_into` read SQLite incremental-blob bytes directly into a caller slice; `read_segments`' three raw arms adopt the resize-then-read-into pattern `BackingAudio` already uses (the base64 arm keeps its input buffer — it's a transform). Half B: the output `&mut Vec<u8>` threads from a per-FUSE-worker `thread_local!` scratch buffer down through `Musefs::read_into` → `read_at_into`/`read_at_with_file_into` → `read_segments` (where the allocation actually lives); the old `Vec`-returning names become thin wrappers. fuser 0.17's `ReplyData::data` already sends a borrowed iovec, so filling a reused buffer and replying from it is the whole win — there is no fuser-layer copy to remove.

**Tech Stack:** Rust, fuser 0.17 (`threadpool` workers), rusqlite incremental blob I/O, Criterion read benches, proptest read-fidelity gate.

**Spec:** `docs/superpowers/specs/2026-06-03-phase6-perf-sps-design.md` ("PR 3").

**Prerequisite:** PR 2 (`phase6-pr2-scan-pair`) is merged to main.

---

### Task 1: Branch

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout main && git pull && git checkout -b phase6-pr3-serve-copies
```

### Task 2: DB chunk readers — `*_into` variants

**Files:**
- Modify: `musefs-db/src/art.rs` (`read_art_chunk`, art.rs:69)
- Modify: `musefs-db/src/tags.rs` (`read_binary_tag_chunk`, tags.rs:140)

- [ ] **Step 1: Write the failing tests**

Append to `musefs-db/tests/art.rs` (which already imports `common::{jpeg, new_track}`):

```rust
#[test]
fn read_art_chunk_into_matches_vec_variant_and_errors_on_short_read() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_art(&jpeg((0u8..64).collect())).unwrap();

    let expected = db.read_art_chunk(id, 3, 5).unwrap();
    let mut buf = vec![0u8; 5];
    db.read_art_chunk_into(id, 3, &mut buf).unwrap();
    assert_eq!(buf, expected);
    assert_eq!(buf, vec![3, 4, 5, 6, 7]);

    // Reading past the blob end must error, not zero-fill (read_at_exact contract).
    let mut over = vec![0u8; 128];
    assert!(db.read_art_chunk_into(id, 3, &mut over).is_err());
}
```

And to `musefs-db/tests/tags.rs`. That file currently imports only `{Db, Tag}` (tags.rs:3) and has no binary-tag tests — extend the import to `use musefs_db::{BinaryTag, Db, Tag};`. `get_binary_tags` returns `BinaryTagRow { rowid, key, byte_len }` (`musefs-db/src/models.rs:191`); the `rowid` is the payload id the chunk reader takes:

```rust
#[test]
fn read_binary_tag_chunk_into_matches_vec_variant_and_errors_on_short_read() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    db.set_binary_tags(
        track,
        &[BinaryTag {
            key: "APIC".into(),
            payload: (0u8..64).collect(),
            ordinal: 0,
        }],
    )
    .unwrap();
    let payload_id = db.get_binary_tags(track).unwrap()[0].rowid;

    let expected = db.read_binary_tag_chunk(payload_id, 3, 5).unwrap();
    let mut buf = vec![0u8; 5];
    db.read_binary_tag_chunk_into(payload_id, 3, &mut buf).unwrap();
    assert_eq!(buf, expected);

    let mut over = vec![0u8; 128];
    assert!(db.read_binary_tag_chunk_into(payload_id, 3, &mut over).is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-db chunk_into`
Expected: FAIL — methods not found.

- [ ] **Step 3: Implement (delegate the Vec variants)**

In `musefs-db/src/art.rs`, replace `read_art_chunk` with the pair:

```rust
/// Stream art-blob bytes at `offset` directly into `buf` via SQLite incremental
/// blob I/O — no intermediate allocation (#70). A short read means the row no
/// longer matches the layout; `read_at_exact` surfaces that as an error rather
/// than silently zero-filling.
pub fn read_art_chunk_into(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()> {
    let blob = self.conn.blob_open("main", "art", "data", art_id, true)?;
    blob.read_at_exact(buf, offset as usize)?;
    Ok(())
}

/// Allocating convenience form of `read_art_chunk_into` (non-hot-path callers).
pub fn read_art_chunk(&self, art_id: i64, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    self.read_art_chunk_into(art_id, offset, &mut buf)?;
    Ok(buf)
}
```

Mirror exactly for `read_binary_tag_chunk_into` / `read_binary_tag_chunk` in `tags.rs` (blob table `"tags"`, column `"value_blob"`, key `payload_id`; keep the existing doc comment's `payload_id` invariant note on the `_into` form).

- [ ] **Step 4: Run tests, commit**

Run: `cargo test -p musefs-db`
Expected: PASS

```bash
git add musefs-db/src/art.rs musefs-db/src/tags.rs
git commit -m "feat(db): chunk readers write into caller buffers (#70)"
```

### Task 3: Thread the output buffer through the reader

**Files:**
- Modify: `musefs-core/src/reader.rs` (`read_at` :406, `read_segments` :427, `read_at_with_file` :531)

The behavior gate is the existing suite: `tests/read_at.rs`, `tests/reader.rs`, `proptest_read_fidelity`, and the `--features metrics` serve-site counter tests — all must pass unchanged (counters fire identically; only buffer destinations change).

- [ ] **Step 1: Convert `read_segments` to fill a caller buffer**

Rename to `read_segments_into`, taking `out: &mut Vec<u8>` (appends; caller clears) and returning `Result<()>`:

```rust
fn read_segments_into(
    resolved: &ResolvedFile,
    db: &Db,
    file: Option<&std::fs::File>,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
```

Body changes from the current version (keep everything else, including all comments and metrics calls):

- Replace the early return `return Ok(Vec::new());` with `return Ok(());`, and `let mut out = Vec::with_capacity(...)` with `out.reserve((end - offset) as usize);`.
- `Segment::ArtImage` arm:

```rust
Segment::ArtImage { art_id, .. } => {
    let start = out.len();
    out.resize(start + n, 0);
    db.read_art_chunk_into(*art_id, within, &mut out[start..])?;
    crate::metrics::on_art_chunk();
}
```

- `Segment::BinaryTag` arm:

```rust
Segment::BinaryTag { payload_id, .. } => {
    let start = out.len();
    out.resize(start + n, 0);
    db.read_binary_tag_chunk_into(*payload_id, within, &mut out[start..])?;
    crate::metrics::on_binary_tag_chunk();
}
```

- `Segment::OggArtSlice` raw (`base64: false`) branch:

```rust
} else {
    // Raw image bytes (OggFLAC PICTURE block).
    let start = out.len();
    out.resize(start + n, 0);
    db.read_art_chunk_into(*art_id, *offset + within, &mut out[start..])?;
    crate::metrics::on_art_chunk();
}
```

- The base64 branch keeps `db.read_art_chunk(...)` + `encode_b64_slice` into `out` unchanged — it transforms raw bytes into base64 output and genuinely needs its input buffer.
- `Inline`, `BackingAudio`, `OggAudio` arms already write into `out`; unchanged.
- Final `Ok(out)` becomes `Ok(())`.

- [ ] **Step 2: Add the `_into` entry points; keep Vec wrappers**

```rust
/// Read `size` bytes at virtual `offset` into `out` (appended), opening the
/// backing file once for this call if the layout needs it.
pub fn read_at_into(
    resolved: &ResolvedFile,
    db: &Db,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    if offset >= resolved.total_len || size == 0 {
        return Ok(());
    }
    let needs_file = resolved
        .layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::BackingAudio { .. } | Segment::OggAudio { .. }));
    if needs_file {
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        read_segments_into(resolved, db, Some(&file), offset, size, out)
    } else {
        read_segments_into(resolved, db, None, offset, size, out)
    }
}

/// Allocating form of `read_at_into` (tests and non-hot-path callers).
pub fn read_at(resolved: &ResolvedFile, db: &Db, offset: u64, size: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    read_at_into(resolved, db, offset, size, &mut out)?;
    Ok(out)
}

/// Serve into `out` from an already-open backing `file` (per-handle path).
pub fn read_at_with_file_into(
    resolved: &ResolvedFile,
    db: &Db,
    file: &std::fs::File,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    read_segments_into(resolved, db, Some(file), offset, size, out)
}

/// Allocating form of `read_at_with_file_into`.
pub fn read_at_with_file(
    resolved: &ResolvedFile,
    db: &Db,
    file: &std::fs::File,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    read_at_with_file_into(resolved, db, file, offset, size, &mut out)?;
    Ok(out)
}
```

(Keep the original doc comments on the moved logic.)

- [ ] **Step 3: Run the reader gates**

```bash
cargo test -p musefs-core --test read_at --test reader
cargo test -p musefs-core --test proptest_read_fidelity
cargo test -p musefs-core --features metrics
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "perf(core): read_segments fills the caller's buffer; chunk arms direct-write (#70)"
```

### Task 4: `Musefs::read_into`

**Files:**
- Modify: `musefs-core/src/facade.rs` (`read`, facade.rs:725)

- [ ] **Step 1: Convert `read` to `read_into` + wrapper**

Rename the existing method to `read_into` with signature:

```rust
/// Serve a read into `out` (cleared first). The FUSE layer passes a reused
/// per-worker buffer so the hot path allocates nothing per read (#70).
pub fn read_into(
    &self,
    inode: u64,
    fh: u64,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
```

Mechanical changes inside the body (its structure — handle lookup, gen-stamp retry loop, binary-tag snapshot guard, inode fallback — stays identical):

- First line of the method: `out.clear();`
- Also `out.clear();` at the **top of each retry iteration** (start of the `for _attempt in 0..4` loop body), so a failed/stale attempt never leaves partial bytes.
- The per-handle serve closure changes from returning `Result<Option<Vec<u8>>>` to `Result<Option<()>>`:
  - `Ok(Some(read_at_with_file(r, db, &h.file, offset, size)?))` becomes
    `{ read_at_with_file_into(r, db, &h.file, offset, size, out)?; Ok(Some(())) }` (both occurrences — the binary-tag-guarded one and the plain one).
  - `if let Some(bytes) = served { return Ok(bytes); }` becomes
    `if served.is_some() { return Ok(()); }`.
- The inode fallback tail becomes:

```rust
self.pool.with(|db| {
    let resolved = self.cache.resolve(db, track_id)?;
    read_at_into(&resolved, db, offset, size, out)
})
```

Then add the compatibility wrapper (existing tests and any non-FUSE callers keep working):

```rust
/// Allocating form of `read_into`.
pub fn read(&self, inode: u64, fh: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    self.read_into(inode, fh, offset, size, &mut out)?;
    Ok(out)
}
```

Borrow note: the closure passed to `self.pool.with` captures `out` mutably while the loop also reads `h` — this is fine (disjoint), but if the compiler objects to `out` being captured by two closures across iterations, take `let out: &mut Vec<u8> = out;` reborrows per use.

- [ ] **Step 2: Run the facade gates**

```bash
cargo test -p musefs-core
```

Expected: PASS (facade tests call `read`, which now wraps `read_into`).

- [ ] **Step 3: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "feat(core): Musefs::read_into serves into a caller buffer (#70)"
```

### Task 5: FUSE per-worker scratch buffer

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (`impl Filesystem for MusefsFs/read`, lib.rs:277)

- [ ] **Step 1: Implement the thread-local buffer**

Near the top of `lib.rs` (module scope, after the imports):

```rust
/// Per-worker read scratch buffer: each threadpool worker reuses one Vec across
/// reads (filled by `Musefs::read_into`, sent as fuser's borrowed iovec), so the
/// hot path allocates nothing per read. Capacity is clamped after use so one
/// giant read doesn't pin memory for the worker's lifetime.
const MAX_RETAINED_READ_BUF: usize = 2 * 1024 * 1024;
thread_local! {
    static READ_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}
```

Replace the `read` method's pool closure:

```rust
fn read(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
) {
    let core = Arc::clone(&self.core);
    self.pool.execute(move || {
        READ_BUF.with(|b| {
            let mut buf = b.borrow_mut();
            match core.read_into(ino.0, fh.0, offset, size as u64, &mut buf) {
                Ok(()) => reply.data(&buf),
                Err(e) => reply.error(errno(&e)),
            }
            if buf.capacity() > MAX_RETAINED_READ_BUF {
                buf.shrink_to(MAX_RETAINED_READ_BUF);
            }
        });
    });
}
```

- [ ] **Step 2: Run the FUSE suites**

```bash
cargo test -p musefs-fuse
cargo test -p musefs-fuse -- --ignored
```

Expected: PASS — `end_to_end_read_through_mount` and `all_supported_formats_decode_to_same_pcm_sha_as_source` are the byte-identical gate for the whole new path.

- [ ] **Step 3: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "perf(fuse): reuse a per-worker read buffer (#70)"
```

### Task 6: Benchmarks — before/after

- [ ] **Step 1: Record "before" on main**

```bash
git checkout main
cargo bench -p musefs-core --bench read_throughput | tee /tmp/read-before.txt
git checkout phase6-pr3-serve-copies
```

- [ ] **Step 2: Record "after"**

```bash
cargo bench -p musefs-core --bench read_throughput | tee /tmp/read-after.txt
```

**Acceptance (the SP3/SP4 gates):**
- `sequential_read` median per format: no format rises >10% (the regression gate); art-bearing fronts should hold or improve.
- `concurrent_read_walk/m16_plus_walker`: held or improved.
- ogg `cold_first_read` / `seek_read`: held (SP4's representative benches).

- [ ] **Step 3: Byte-identical + fuzz-feature gates**

```bash
cargo test -p musefs-core --test proptest_read_fidelity
cargo test -p musefs-format --features fuzzing
```

Expected: PASS.

- [ ] **Step 4: BENCHMARKS.md + ROADMAP, commit**

Add `## Phase 6 PR 3 — Serve-path copies (#70)` to `BENCHMARKS.md` (per-format `sequential_read` medians before/after, `concurrent_read_walk`, ogg cold/seek, reproduce commands, and a note that fuser 0.17 already sends a borrowed iovec — the win is chunk direct-write + allocation elimination, not a fuser-layer copy). Strike through #70 in `docs/ROADMAP.md` Phase 6.

```bash
git add BENCHMARKS.md docs/ROADMAP.md
git commit -m "bench/docs: record serve-path before/after; mark #70 done"
```

### Task 7: Validation gates + PR

- [ ] **Step 1: Format, lint, full tests**

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test --workspace
```

Expected: all clean.

- [ ] **Step 2: In-diff mutation gate (CI parity)**

```bash
cd /home/cfutro/git/musefs
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -c '^diff --git ' mutants.diff && grep -c '^@@ ' mutants.diff   # sanity: non-empty
TMPDIR=/home/cfutro/.cache/mutants-tmp cargo mutants --in-diff mutants.diff -j4 \
  --exclude 'musefs-latencyfs/**' --output mutants-out/in-diff
cat mutants-out/in-diff/mutants.out/missed.txt
rm -rf /home/cfutro/.cache/mutants-tmp mutants-out mutants.diff
```

Expected: 0 missed (kill survivors with targeted tests, commit, regenerate the diff, re-run).

- [ ] **Step 3: Fuzz-target build check**

The fuzz crate is outside the workspace; reader-facing signature changes only surface in CI's smoke job unless checked locally:

```bash
cargo +nightly fuzz build flac 2>/dev/null || echo "fuzz targets touch format-layer only; verify no reader symbols leaked into fuzz_check"
```

(Only needed if `musefs-format` or `fuzz_check` surfaces changed — this PR shouldn't touch them; skip if the diff confirms that.)

- [ ] **Step 4: Push, open the PR, correct the issue**

```bash
git push -u origin phase6-pr3-serve-copies
gh pr create --title "Phase 6 PR 3: serve-path copy elimination (#70)" --body "$(cat <<'EOF'
Closes #70.

Half A: art/binary-tag chunk reads land directly in the output buffer's
reserved tail via new Db::read_*_chunk_into (the base64 Ogg-art arm keeps
its input buffer — it's a transform). Half B: the output buffer threads
from a per-FUSE-worker thread_local scratch Vec down through
Musefs::read_into -> read_at_into -> read_segments_into, eliminating the
per-read allocation; capacity is clamped at 2 MiB after use.

Scope note: fuser 0.17's ReplyData::data already sends a borrowed iovec
(ResponseSlice -> with_iovec), so the issue's second claimed copy ("into
the kernel's FUSE reply buffer") does not exist at this fuser version; the
remaining kernel-boundary writev is not eliminable from userspace.

Bench: BENCHMARKS.md "Phase 6 PR 3" (sequential_read per format,
concurrent_read_walk, ogg cold/seek — all within the >10% gate).
Spec: docs/superpowers/specs/2026-06-03-phase6-perf-sps-design.md.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Then post the correction comment on the issue:

```bash
gh issue comment 70 --body "Scope correction verified during implementation: fuser 0.17's ReplyData::data sends the payload as a borrowed iovec (ResponseSlice -> with_iovec -> vectored write), so the second copy described here ('copied again into the kernel's FUSE reply buffer') does not exist at our fuser version. The PR eliminates the two copies userspace controls: art/binary-tag chunk intermediate buffers and the per-read output Vec allocation."
```
