# Phase 6 PR 2 — Scan pair: lazy ID3v1 tail (#67) + move-not-clone ingest (#68) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop paying an unused 128-byte ID3v1 tail read for every non-MP3 file during scan (#67), and stop cloning picture/binary-tag/structural-block bytes during bulk ingest (#68).

**Architecture:** Both fixes live in `musefs-core/src/scan.rs`. #67: `probe_prefix` dispatches by extension, so `probe_file` gates the eager `read_tail_128` on `has_ext(path, "mp3")` — no signature changes. #68: the pipeline batch owns its `Probed`s; `ingest_bulk` takes them by value (the writer `drain`s the batch) and moves the byte buffers into the DB structs, with each unit's byte-budget `weight` captured before the move because release happens after `bw.commit()`.

**Tech Stack:** Rust; existing `bench_ingest` harness + `--features metrics` scan counters; bounded≡full probe-equivalence gate.

**Spec:** `docs/superpowers/specs/2026-06-03-phase6-perf-sps-design.md` ("PR 2").

**Prerequisite:** PR 1 (`phase6-pr1-incremental-refresh`) is merged to main.

---

### Task 1: Branch

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout main && git pull && git checkout -b phase6-pr2-scan-pair
```

### Task 2: #67 — gate the tail read on the MP3 extension

**Files:**
- Modify: `musefs-core/src/scan.rs` (`probe_file`, scan.rs:204)
- Test: `musefs-core/tests/metrics.rs` (already `#![cfg(feature = "metrics")]` with the `METRICS_LOCK` serialization pattern — the scan counters are global statics)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/metrics.rs` (hold `METRICS_LOCK` exactly like its neighbors):

```rust
/// #67: only .mp3 consumes the ID3v1 tail; non-MP3 formats must not pay the
/// 128-byte tail read. A 300-byte FLAC (< the 1 MiB window) probes in exactly
/// one positioned read of exactly the file's length.
#[test]
fn scan_reads_no_id3v1_tail_for_flac() {
    let _guard = METRICS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // Minimal valid FLAC padded to 300 bytes: marker + last STREAMINFO + audio.
    let mut b = b"fLaC".to_vec();
    b.push(0x80); // last-block flag | STREAMINFO
    b.extend_from_slice(&[0, 0, 34]);
    b.extend(std::iter::repeat_n(0u8, 34));
    b.extend(std::iter::repeat_n(0x55u8, 300 - b.len())); // audio payload
    let path = dir.path().join("t.flac");
    std::fs::write(&path, &b).unwrap();
    let len = std::fs::metadata(&path).unwrap().len();

    let db = musefs_db::Db::open_in_memory().unwrap();
    metrics::reset();
    scan_directory(&db, dir.path()).unwrap();
    let s = metrics::snapshot();
    assert_eq!(s.scan_preads, 1, "flac: one bounded prefix read, no tail read");
    assert_eq!(s.scan_bytes_read, len, "no +128 ID3v1 tail for non-mp3");
}

/// #67 inverse: MP3 keeps its tail read (prefix + 128-byte ID3v1 trailer).
#[test]
fn scan_still_reads_id3v1_tail_for_mp3() {
    let _guard = METRICS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use common::corpus::{prepare_format, CorpusParams, Format as CFormat};
    let tmp = tempfile::tempdir().unwrap();
    let params = CorpusParams::single(CFormat::Mp3, 1, 1);
    let target = prepare_format(&params, tmp.path(), params.format_mix[0]);
    let mp3 = std::fs::read_dir(&target.corpus_dir)
        .unwrap()
        .flat_map(|e| {
            let p = e.unwrap().path();
            if p.is_dir() {
                std::fs::read_dir(p).unwrap().map(|e| e.unwrap().path()).collect()
            } else {
                vec![p]
            }
        })
        .find(|p| p.extension().is_some_and(|x| x == "mp3"))
        .expect("generated mp3");
    let len = std::fs::metadata(&mp3).unwrap().len();

    let db = musefs_db::Db::open_in_memory().unwrap();
    metrics::reset();
    scan_directory(&db, &target.corpus_dir).unwrap();
    let s = metrics::snapshot();
    // Corpus tracks are far below the default 1 MiB scan window (this test must
    // not set MUSEFS_SCAN_WINDOW): one prefix read + the tail. read_tail_128
    // always reads 128 bytes when file_len >= 128, trailer present or not, so
    // the +128 assertion is robust.
    assert_eq!(s.scan_preads, 2, "mp3: prefix read + ID3v1 tail read");
    assert_eq!(s.scan_bytes_read, len + 128, "mp3 keeps the 128-byte tail");
}
```

Adapt the corpus-walk to `common::corpus`'s actual layout helpers if it exposes the generated file paths directly (check `Target`'s fields); the assertions are the contract. If the generated MP3 needs a widen retry (it shouldn't at these sizes), pin the byte arithmetic to the observed pre-change baseline minus 128 for FLAC — the invariant under test is *the 128-byte delta between formats*.

- [ ] **Step 2: Run tests to verify the FLAC one fails**

Run: `cargo test -p musefs-core --features metrics --test metrics scan_reads_no_id3v1_tail`
Expected: FAIL — `scan_preads` is 2 and `scan_bytes_read` is `len + 128` (the tail is still read). The MP3 test passes before AND after (it pins current behavior).

- [ ] **Step 3: Implement**

In `probe_file` (scan.rs:204), replace:

```rust
    // Front-anchored formats: read a window, widen on NeedMore.
    let tail = read_tail_128(&file, file_len)?;
```

with:

```rust
    // Front-anchored formats: read a window, widen on NeedMore. Only the MP3
    // arm of probe_prefix consumes the ID3v1 tail, and dispatch is by
    // extension — so only .mp3 pays the tail read (#67).
    let tail = if has_ext(path, "mp3") {
        read_tail_128(&file, file_len)?
    } else {
        None
    };
```

Nothing else changes — `tail` keeps its `Option<[u8; 128]>` type, the widen-retry loop reuses it, and MP3 I/O-error propagation is unchanged.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Run: `cargo test -p musefs-core --test probe_equivalence`
Run: `cargo test -p musefs-core scan`
Expected: PASS — including the bounded≡full equivalence gate.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/metrics.rs
git commit -m "perf(scan): read the ID3v1 tail only for .mp3 files (#67)"
```

### Task 3: #68 — drain the batch, move the bytes

**Files:**
- Modify: `musefs-core/src/scan.rs` (`ingest_bulk` scan.rs:489, the `flush` closure in `run_pipeline` scan.rs:667)

This is a behavior-preserving refactor; the gate is the existing suite (scan tests, `pipeline_backpressure`, `probe_equivalence`, PCM e2e) plus the mutation gate in Task 5. No new tests.

- [ ] **Step 1: Change `ingest_bulk` to take `Probed` by value**

Replace the signature and the three cloning blocks:

```rust
/// Like `ingest`, but writes through a batch `BulkWriter`. Takes `probed` by
/// value so picture/binary-tag/structural-block bytes are moved, not cloned (#68).
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    meta_len: u64,
    meta_mtime: i64,
    probed: Probed,
) -> Result<()> {
    let track_id = bw.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset as i64,
        audio_length: probed.audio_length as i64,
        backing_size: meta_len as i64,
        backing_mtime: meta_mtime,
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, i64> = HashMap::new();
    for (key, value) in &probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(key, value, *ord));
        *ord += 1;
    }
    bw.replace_tags(track_id, &tags)?;

    let binary_tags: Vec<musefs_db::BinaryTag> = probed
        .binary_tags
        .into_iter()
        .filter(|b| !b.payload.is_empty() && b.payload.len() <= MAX_BINARY_TAG_BYTES)
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag {
            key: b.key,
            payload: b.payload,
            ordinal: ordinal as i64,
        })
        .collect();
    bw.set_binary_tags(track_id, &binary_tags)?;

    let mut sb_ordinals: HashMap<String, i64> = HashMap::new();
    let structural_blocks: Vec<musefs_db::StructuralBlock> = probed
        .structural_blocks
        .into_iter()
        .map(|(kind, body)| {
            let ord = sb_ordinals.entry(kind.clone()).or_insert(0);
            let sb = musefs_db::StructuralBlock {
                kind,
                ordinal: *ord,
                body,
            };
            *ord += 1;
            sb
        })
        .collect();
    bw.set_structural_blocks(track_id, &structural_blocks)?;

    let mut track_arts = Vec::new();
    let accepted = probed
        .pictures
        .into_iter()
        .filter(|p| p.data.len() <= MAX_ART_BYTES);
    for (ordinal, pic) in accepted.enumerate() {
        let art_id = bw.upsert_art(&NewArt {
            mime: pic.mime,
            width: (pic.width != 0).then_some(pic.width as i64),
            height: (pic.height != 0).then_some(pic.height as i64),
            data: pic.data,
        })?;
        let picture_type = if pic.picture_type <= 20 {
            pic.picture_type as i64
        } else {
            0
        };
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description,
            ordinal: ordinal as i64,
        });
    }
    bw.set_track_art(track_id, &track_arts)?;
    Ok(())
}
```

Subtleties:
- `sb_ordinals` note: the `kind.clone()` for the ordinal key stays (the kind `String` itself is moved into the struct); kinds are short ASCII names, not payload bytes.
- The `StructuralBlock` arm assumes `structural_blocks: Vec<(String, Vec<u8>)>` on `Probed` — adjust the destructuring to the actual element type if it differs (check the struct at scan.rs:85ish), keeping the move semantics.
- If `Tag::new` borrows, the text-tags loop is unchanged (text tags are small; the issue is byte payloads).

- [ ] **Step 2: Update the `flush` closure to drain by value, capturing weights first**

In `run_pipeline` (scan.rs:667), replace the `flush` closure body:

```rust
let flush = |batch: &mut Vec<Unit>, batch_bytes: &mut u64, scanned: &mut u64| -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut bw = db.bulk_writer()?;
    // Budget weights are released only after commit, and ingest_bulk consumes
    // the Probed — capture each unit's weight before the move (#68).
    let mut weights = Vec::with_capacity(batch.len());
    for u in batch.drain(..) {
        weights.push(u.weight);
        ingest_bulk(&mut bw, &u.abs_path, u.meta_len, u.meta_mtime, u.probed)?;
        *scanned += 1;
    }
    bw.commit()?;
    for w in weights {
        budget.release(w);
    }
    *batch_bytes = 0;
    Ok(())
};
```

(A mid-drain `ingest_bulk` error aborts the whole scan via `?` exactly as before; the un-released weights on that path match today's semantics — the process-scoped budget dies with the scan.)

Note `u.abs_path` is moved-from territory once `u` is destructured — the loop above moves `u.probed` while borrowing `u.abs_path`; if the borrow checker objects, destructure explicitly:

```rust
    for Unit { abs_path, meta_len, meta_mtime, probed, weight } in batch.drain(..) {
        weights.push(weight);
        ingest_bulk(&mut bw, &abs_path, meta_len, meta_mtime, probed)?;
        *scanned += 1;
    }
```

- [ ] **Step 3: Fix any other `ingest_bulk` caller**

Run: `cargo build -p musefs-core`
If `revalidate_with` or the single-file path also calls `ingest_bulk` with a borrow, pass ownership there too (clone only if the caller genuinely retains the `Probed` afterward — it shouldn't). The non-bulk `ingest` function is out of scope (issue #68 is the bulk path).

- [ ] **Step 4: Run the gates**

```bash
cargo test -p musefs-core
cargo test -p musefs-core --features metrics
cargo test -p musefs-core --test pipeline_backpressure
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "perf(scan): move ingest payload bytes instead of cloning (#68)"
```

### Task 4: Benchmarks — before/after

- [ ] **Step 1: Record "before" on main**

```bash
git checkout main
cargo test -p musefs-core --release --features metrics --test bench_ingest -- --ignored --nocapture \
  | tee /tmp/ingest-before.txt
git checkout phase6-pr2-scan-pair
```

- [ ] **Step 2: Record "after"**

```bash
cargo test -p musefs-core --release --features metrics --test bench_ingest -- --ignored --nocapture \
  | tee /tmp/ingest-after.txt
```

**Acceptance:** non-MP3 formats show `preads` down by ~1/file and `bytes_read` down by 128 B/file (#67); wall time held-or-improved across all formats (no >10% rise). #68's win shows in wall/RSS on the art-bearing corpora; flat-within-noise is acceptable for the small `ci` tier — the clone elimination is structural.

- [ ] **Step 3: Write the BENCHMARKS.md section**

Add `## Phase 6 PR 2 — Scan pair (#67, #68)` at the end of `BENCHMARKS.md`: per-format before/after table (wall, opens, preads, bytes_read), the per-file pread/byte deltas called out, reproduce commands.

- [ ] **Step 4: Update ROADMAP and commit**

Strike through #67 and #68 in `docs/ROADMAP.md`'s Phase 6 list (Phase 0–5 style, one-line summaries).

```bash
git add BENCHMARKS.md docs/ROADMAP.md
git commit -m "bench/docs: record scan-pair before/after; mark #67/#68 done"
```

### Task 5: Validation gates + PR

- [ ] **Step 1: Format, lint, full tests**

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test --workspace
```

Expected: all clean.

- [ ] **Step 2: Scan-side e2e**

```bash
cargo test -p musefs-core --test probe_equivalence
cargo test -p musefs-fuse -- --ignored
```

Expected: PASS (PCM-sha byte-identical gate).

- [ ] **Step 3: Re-anchor the scan.rs mutants.toml exclusions (PR 1 learning)**

`.cargo/mutants.toml` suppresses ~11 proven-equivalent scan.rs mutants by
**file:line:col** (probe_file widen loop 225:31/232:30, run_pipeline
sync_channel 493:46, flush cadence 573:29–585:70). The Task 2 edit inserts
lines *above* 225 and the Task 3 rewrite changes ingest_bulk's length *above*
573 — every shifted anchor silently stops matching and its known-equivalent
mutant resurfaces as MISSED in the gate. After Tasks 2–3 are committed,
re-derive each anchor's new line:col (the per-class justification comments
identify the sites) and commit the mutants.toml update before running the
gate. Do NOT blanket-convert these to description anchors: the file's header
comment notes killable siblings share the same descriptions (e.g. other
`+`->`*` sites in scan.rs); keep line:col here, description-anchoring only
where the description is unique (see the PR 1 tree.rs/facade.rs entries for
the pattern).

- [ ] **Step 4: In-diff mutation gate (CI parity)**

```bash
cd /home/cfutro/git/musefs
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -c '^diff --git ' mutants.diff && grep -c '^@@ ' mutants.diff   # sanity: non-empty
TMPDIR=/home/cfutro/.cache/mutants-tmp cargo mutants --in-diff mutants.diff -j$(nproc) \
  --exclude 'musefs-latencyfs/**' --output mutants-out/in-diff
cat mutants-out/in-diff/mutants.out/missed.txt
rm -rf /home/cfutro/.cache/mutants-tmp mutants-out mutants.diff
```

Expected: 0 missed AND 0 timeouts — CI's gate fails on either (PR 1 learning:
a mutation that turns a bounded loop into a spin shows up as TIMEOUT, exit
code 3; probe_file's widen-retry loop is in this diff's blast radius, so a new
hang-class mutant gets a justified mutants.toml exclusion per the SP3/PR 1
precedent, not a test). The likely survivor class is the `has_ext(path,
"mp3")` gate (`== → !=` style); the two Task 2 counter tests are its killers —
if one survives anyway, pin the exact byte count in the test. Note the gate
runs the **mutated crate's own** test suite: scan.rs mutants need musefs-core
tests (true here; PR 1 was bitten by this with a musefs-db mutant).

- [ ] **Step 5: Push and open the PR**

```bash
git push -u origin phase6-pr2-scan-pair
gh pr create --title "Phase 6 PR 2: scan perf pair (#67, #68)" --body "$(cat <<'EOF'
Closes #67, closes #68.

#67: probe_file gates the eager ID3v1 tail read on the .mp3 extension —
the only consumer — saving one syscall + 128 B per non-MP3 file.
#68: the bulk-ingest writer drains its batch by value, moving picture/
binary-tag/structural-block bytes into the DB structs instead of cloning;
budget weights are captured before the move (release-after-commit order
preserved).

Bench: BENCHMARKS.md "Phase 6 PR 2" (per-format bench_ingest before/after).
Spec: docs/superpowers/specs/2026-06-03-phase6-perf-sps-design.md.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
