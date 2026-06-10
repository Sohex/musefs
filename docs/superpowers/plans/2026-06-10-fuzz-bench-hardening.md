# Fuzz/bench Harness Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the fuzz and bench harnesses across four issues (#216, #212, #213, #197): durable crash-reproducer replay, differential fuzzing of the bounded probers, fuzzing + proptesting the read-time serve path, and three small polish fixes.

**Architecture:** Most changes live in the out-of-workspace `fuzz/` crate (libFuzzer targets) and are verified with `cargo +nightly fuzz build`/`run`. The behavioral additions — the mp3 prober-equivalence unit test, the Ogg read-fidelity proptest, and the mp3 binary-frame fixture — live in the workspace and are gated by the pre-commit hook's full `cargo test`. The serve fuzz target adds a `musefs-core`+`musefs-db` dependency to the fuzz crate.

**Tech Stack:** Rust, `cargo-fuzz`/libFuzzer (nightly), `proptest`, `arbitrary`, SQLite (`musefs-db`), GitHub Actions.

---

## Background the implementer must know

- **The fuzz crate is its own workspace** (`fuzz/Cargo.toml` ends with `[workspace]`). It is NOT built by `cargo build`/`clippy`/`test` at the repo root. Build it with `cargo +nightly fuzz build [target]` from the repo root (cargo-fuzz finds `fuzz/`). Run one target with `cargo +nightly fuzz run <target> -- <libfuzzer-args>`.
- **The pre-commit hook** runs `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, the **full workspace `cargo test`**, and ruff. It does NOT run the fuzz crate. A red workspace test rejects the commit, so every workspace-test commit must be green.
- **`cargo +nightly fuzz` requires the nightly toolchain and `cargo-fuzz` installed.** If `cargo +nightly fuzz build` fails with "no such subcommand", run `cargo install cargo-fuzz` first. If nightly is missing, `rustup toolchain install nightly`.
- **Shared fuzz helpers** live in `fuzz/src/lib.rs` (crate `musefs_fuzz`): `MAX_INPUT` (128 KiB), `arb_tags`, `arb_arts`, `arb_binary_tags`.
- **`Extent<T>`** is re-exported at `musefs_format::Extent` (defined in `musefs-format/src/probe.rs`): variants `Complete(T)` and `NeedMore { up_to: u64 }`.
- **Format result types all derive `PartialEq`** (verified): `FlacMeta`, `Mp3Bounds`, `WavBounds`, `Mp4Scan`, `OggHeader`. No new derives needed.

When a fuzz-only change has no workspace test, the verification step is a build plus a short corpus run; commit after it passes.

---

## File Structure

**Workstream D — #197 polish**
- Modify: `fuzz/fuzz_targets/b64.rs` — add labelled OOB assert.
- Modify: `musefs-core/benches/read_throughput.rs:148-157` — reorder reader/walker join unwrap.
- Modify: `musefs-format/src/fuzz_check.rs` — add `fixtures::mp3_with_binary_frame()` + a `fixtures_tests` test.
- Modify: `fuzz/src/bin/generate_seeds.rs:23` — use the new builder for the mp3 `seed_binary`.

**Workstream B — #212 bounded differential oracle**
- Modify: `musefs-format/src/mp3.rs` (`#[cfg(test)] mod tests`) — add prober-equivalence unit test.
- Modify each existing target to drive its bounded twin: `fuzz/fuzz_targets/{mp3,flac,wav,ogg,mp4}.rs`.

**Workstream C — #213 serve fuzzing + Ogg proptest**
- Modify: `musefs-core/tests/proptest_read_fidelity.rs` — add `build_ogg`, `build_ogg_with_art`, Ogg `proptest!` block.
- Modify: `fuzz/Cargo.toml` — add `musefs-core`, `musefs-db`, `tempfile` deps + `[[bin]] serve`.
- Create: `fuzz/fuzz_targets/serve.rs` — the serve-path target.
- Modify: `fuzz/src/bin/generate_seeds.rs` — add `serve` seeds.

**Workstream A — #216 regression durability**
- Create: `fuzz/regressions/<target>/.gitkeep` (one per target).
- Modify: `.github/workflows/fuzz.yml` — add `regressions` replay job; add `serve` to the `scheduled` matrix (NOT the smoke loop).
- Modify: `CONTRIBUTING.md` — document the regressions convention + new coverage.

---

## Workstream D — #197 polish items

### Task 1: b64 harness labelled OOB assert

**Files:**
- Modify: `fuzz/fuzz_targets/b64.rs:38-43`

- [ ] **Step 1: Add the assert before the slice**

In `fuzz/fuzz_targets/b64.rs`, the current code is:

```rust
    let win = b64_window(out_off, take, img.len() as u64);
    let windowed = encode_b64_slice(
        &img[win.in_start as usize..(win.in_start + win.in_len) as usize],
        win.skip,
        take as usize,
    );
```

Insert an assert between the `b64_window` call and the `encode_b64_slice` call so the slice is provably in-bounds:

```rust
    let win = b64_window(out_off, take, img.len() as u64);
    assert!(
        win.in_start + win.in_len <= img.len() as u64,
        "b64_window returned OOB range: in_start={} in_len={} img_len={}",
        win.in_start,
        win.in_len,
        img.len(),
    );
    let windowed = encode_b64_slice(
        &img[win.in_start as usize..(win.in_start + win.in_len) as usize],
        win.skip,
        take as usize,
    );
```

- [ ] **Step 2: Build the target**

Run: `cargo +nightly fuzz build b64`
Expected: compiles cleanly (no errors).

- [ ] **Step 3: Short corpus run to confirm no regression**

Run: `cargo +nightly fuzz run b64 -- -runs=2000 -max_total_time=15`
Expected: exits 0, "Done 2000 runs" or time-limited; no crash artifact written.

- [ ] **Step 4: Commit**

```bash
git add fuzz/fuzz_targets/b64.rs
git commit -m "test(fuzz): label b64_window OOB range in the b64 harness assert (#197)"
```

### Task 2: bench reader/walker join reorder

**Files:**
- Modify: `musefs-core/benches/read_throughput.rs:148-157`

- [ ] **Step 1: Reorder the unwraps and update the comment**

The current block is:

```rust
            // Join all readers first, then stop the walker — collecting results
            // before unwrapping so a reader panic can't leave the walker spinning
            // (stop is always set before we re-raise the panic).
            let reader_results: Vec<_> =
                readers.into_iter().map(thread::JoinHandle::join).collect();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            walker.join().unwrap();
            for r in reader_results {
                r.unwrap();
            }
```

Replace it with (re-raise reader panics BEFORE joining the walker, so a panicking walker join can't mask a reader-thread panic; `stop` is still set first so the walker can't spin):

```rust
            // Collect reader results, signal stop, then re-raise any reader panic
            // BEFORE joining the walker — a panicking `walker.join().unwrap()`
            // must not mask an original reader-thread panic. `stop` is set before
            // we re-raise, so re-raising first cannot leave the walker spinning.
            let reader_results: Vec<_> =
                readers.into_iter().map(thread::JoinHandle::join).collect();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            for r in reader_results {
                r.unwrap();
            }
            walker.join().unwrap();
```

- [ ] **Step 2: Compile the bench (it only builds under `--all-targets`/clippy)**

Run: `cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: compiles with no warnings.

- [ ] **Step 3: Smoke-run the bench briefly to confirm it still runs**

Run: `cargo bench -p musefs-core --bench read_throughput -- --warm-up-time 1 --measurement-time 1 --sample-size 10`
Expected: the benchmark group runs and reports timings; process exits 0.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/benches/read_throughput.rs
git commit -m "test(bench): re-raise reader panics before joining the walker (#197)"
```

### Task 3: mp3 binary-frame fixture + seed

**Files:**
- Modify: `musefs-format/src/fuzz_check.rs` (`pub mod fixtures` around line 257, and `#[cfg(test)] mod fixtures_tests` around line 366)
- Modify: `fuzz/src/bin/generate_seeds.rs:17-23`

- [ ] **Step 1: Write the failing fixture test**

In `musefs-format/src/fuzz_check.rs`, inside `mod fixtures_tests` (after the existing `m4a_fixture_parses` test), add:

```rust
    #[test]
    fn mp3_with_binary_frame_parses_and_carries_binary_tag() {
        let f = fixtures::mp3_with_binary_frame();
        // The file is a well-formed MP3: locate_audio accepts it.
        let bounds = crate::mp3::locate_audio(&f).unwrap();
        assert!(bounds.audio_length >= 1);
        // It carries a binary (non-text, non-APIC) ID3 frame, so read_binary_tags
        // returns a non-empty opaque list — this is the synthesis path the seed
        // is meant to reach immediately.
        let (opaque, _promoted) = crate::mp3::read_binary_tags(&f);
        assert!(!opaque.is_empty(), "expected an opaque binary ID3 frame");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --features fuzzing fixtures_tests::mp3_with_binary_frame -- --nocapture`
Expected: FAIL — `no function or associated item named mp3_with_binary_frame`.

- [ ] **Step 3: Implement the fixture builder**

In `musefs-format/src/fuzz_check.rs`, inside `pub mod fixtures`, directly after the existing `pub fn mp3() -> Vec<u8>` function, add:

```rust
    /// A well-formed ID3v2.4 MP3 carrying one binary `GEOB` frame (General
    /// Encapsulated Object) ahead of the audio. `GEOB` is not a text/`COMM`/
    /// `USLT`/`APIC` frame, so `mp3::read_binary_tags` classifies it as opaque —
    /// exercising the binary-tag synthesis path immediately, without the fuzzer
    /// having to mutate its way there from the empty-tag `mp3()` seed.
    pub fn mp3_with_binary_frame() -> Vec<u8> {
        // ID3v2.4 frame size is synchsafe (7 bits/byte).
        fn synchsafe(v: u32) -> [u8; 4] {
            [
                ((v >> 21) & 0x7F) as u8,
                ((v >> 14) & 0x7F) as u8,
                ((v >> 7) & 0x7F) as u8,
                (v & 0x7F) as u8,
            ]
        }
        // GEOB body: a minimal, structurally-valid General Encapsulated Object
        // (text-encoding byte, empty MIME/filename/description C-strings, payload).
        let mut body = Vec::new();
        body.push(0x00); // text encoding: ISO-8859-1
        body.push(0x00); // MIME type: empty C-string
        body.push(0x00); // filename: empty C-string
        body.push(0x00); // content description: empty C-string
        body.extend_from_slice(b"musefs-fuzz-binary-seed");

        let mut frame = Vec::new();
        frame.extend_from_slice(b"GEOB");
        frame.extend_from_slice(&synchsafe(body.len() as u32)); // v2.4 synchsafe size
        frame.extend_from_slice(&[0x00, 0x00]); // frame flags
        frame.extend_from_slice(&body);

        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.push(0x04); // major version 4
        out.push(0x00); // revision 0
        out.push(0x00); // flags
        out.extend_from_slice(&synchsafe(frame.len() as u32)); // tag body size
        out.extend_from_slice(&frame);
        // MPEG frame sync (matches `mp3()`): 0xFF 0xFB + 2 bytes.
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --features fuzzing fixtures_tests::mp3_with_binary_frame -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Use the builder for the mp3 `seed_binary`**

In `fuzz/src/bin/generate_seeds.rs`, replace the existing `seed_binary` block:

```rust
    // A second, identically-valid MP3 seed labeled for the binary-tag synthesis
    // path. fixtures::mp3() is the only MP3 builder (no parameterized/longer
    // variant exists), and a corrupt seed would make locate_audio reject the
    // file and skip synthesize_layout entirely — so reuse the valid fixture.
    // The fuzzer reaches non-empty arb_binary_tags via mutation from here.
    write("mp3", "seed_binary", &fixtures::mp3());
```

with:

```rust
    // An MP3 seed that already carries a binary GEOB ID3 frame, so the binary-tag
    // synthesis path gets immediate coverage instead of waiting for the fuzzer to
    // mutate its way to a non-empty arb_binary_tags from the empty-tag seed0.
    write("mp3", "seed_binary", &fixtures::mp3_with_binary_frame());
```

- [ ] **Step 6: Regenerate seeds and confirm the new seed differs from seed0**

Run:
```bash
( cd fuzz && cargo run --bin generate_seeds )
cmp -s fuzz/corpus/mp3/seed0 fuzz/corpus/mp3/seed_binary && echo "IDENTICAL (BAD)" || echo "DIFFER (GOOD)"
```
Expected: prints `DIFFER (GOOD)`.

Note: `fuzz/` is in the root workspace's `exclude` list (it is its own workspace), so `cargo run -p musefs-fuzz` from the repo root fails with "did not match any packages". Always run the generator from inside `fuzz/`.

- [ ] **Step 7: Commit**

```bash
git add musefs-format/src/fuzz_check.rs fuzz/src/bin/generate_seeds.rs fuzz/corpus/mp3/seed_binary
git commit -m "test(fuzz): seed mp3 binary-tag path with a real GEOB frame fixture (#197)"
```

---

## Workstream B — #212 bounded differential oracle

Each existing per-format target gains a differential check against its bounded twin, run on the whole buffer (`file_len = data.len() as u64`). Add `use musefs_format::Extent;` to each target that matches on `Extent`.

### Task 4: mp3 prober-equivalence unit test + bounded oracle

**Files:**
- Modify: `musefs-format/src/mp3.rs` (`#[cfg(test)] mod tests`)
- Modify: `fuzz/fuzz_targets/mp3.rs`

- [ ] **Step 1: Write the failing equivalence unit test**

In `musefs-format/src/mp3.rs`, inside `#[cfg(test)] mod tests` (which already exists and has `use super::*;`), add:

```rust
    /// On a whole buffer with the production tail (`Some(last 128 bytes)` when
    /// the file is at least 128 bytes), `locate_audio_bounded` must agree with
    /// `locate_audio`: same accept/reject, same `Mp3Bounds`. This pins the
    /// equivalence the #212 fuzz oracle relies on.
    fn assert_mp3_bounded_matches_full(data: &[u8]) {
        let len = data.len() as u64;
        let tail: Option<&[u8; 128]> = if data.len() >= 128 {
            data[data.len() - 128..].try_into().ok()
        } else {
            None
        };
        match (locate_audio(data), locate_audio_bounded(data, len, tail)) {
            (Ok(full), Ok(Extent::Complete(bounded))) => assert_eq!(full, bounded),
            (Err(_), Err(_)) => {}
            (full, bounded) => panic!("mp3 bounded/full divergence: full={full:?} bounded={bounded:?}"),
        }
    }

    #[test]
    fn mp3_bounded_matches_full_on_whole_buffer() {
        // Plain ID3v2.4 + frame sync (no trailer, < 128 bytes -> tail None).
        assert_mp3_bounded_matches_full(&crate::fuzz_check::fixtures::mp3());
        // Carries a GEOB frame; longer file.
        assert_mp3_bounded_matches_full(&crate::fuzz_check::fixtures::mp3_with_binary_frame());

        // A >=128-byte MP3 with a trailing ID3v1 "TAG" block, so the tail-strip
        // path is exercised and the tail argument is Some.
        let mut with_trailer = crate::fuzz_check::fixtures::mp3();
        with_trailer.resize(200, 0x00);
        with_trailer.extend_from_slice(b"TAG");
        with_trailer.resize(with_trailer.len() + 125, 0x00); // pad ID3v1 to 128 bytes
        assert_mp3_bounded_matches_full(&with_trailer);
    }
```

Note on imports: `mod tests` already has `use super::*;`, and `mp3.rs` imports `Extent` at the top (`use crate::probe::Extent;` — line 5), so `Extent` is already in scope inside `mod tests`. Do **not** add another `use crate::Extent;` — a redundant import trips the pre-commit `-D warnings` gate. This also depends on Task 3 (it adds `fixtures::mp3_with_binary_frame()`), so do Task 3 first.

- [ ] **Step 2: Run the characterization test — it should PASS immediately**

This is a characterization test that pins the *existing* `mp3.rs` behavior (it asserts the bounded and full probers already agree), so it passes on the first green run — there is no red-to-green cycle here.

Run: `cargo test -p musefs-format --features fuzzing mp3_bounded_matches_full -- --nocapture`
Expected: PASS.

If instead it fails on an **assertion** (not a compile error), STOP: the bounded/full equivalence is false, so the #212 mp3 fuzz oracle in Step 3 must drop to weak-invariants-only (panic-freedom + `up_to <= file_len`) instead of `assert_eq!`. If it fails to **compile** with "no function `mp3_with_binary_frame`", Task 3 has not been done yet — do it first.

- [ ] **Step 3: Add the bounded oracle to the mp3 fuzz target**

In `fuzz/fuzz_targets/mp3.rs`, change the imports line:

```rust
use musefs_format::{fuzz_check::assert_backing_covers_audio, mp3};
```
to:
```rust
use musefs_format::{fuzz_check::assert_backing_covers_audio, mp3, Extent};
```

Then, immediately after the existing block that binds `bounds`:

```rust
    let bounds = match mp3::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
```

insert:

```rust
    // #212: the bounded twin must agree with the full parse on a whole buffer.
    let len = data.len() as u64;
    let tail: Option<&[u8; 128]> = if data.len() >= 128 {
        data[data.len() - 128..].try_into().ok()
    } else {
        None
    };
    match mp3::locate_audio_bounded(data, len, tail) {
        Ok(Extent::Complete(bb)) => assert_eq!(bb, bounds, "mp3 bounded != full"),
        other => panic!("mp3 bounded diverged from full Ok: {other:?}"),
    }
```

- [ ] **Step 4: Build and short-run the target**

Run: `cargo +nightly fuzz build mp3 && cargo +nightly fuzz run mp3 -- -runs=5000 -max_total_time=20`
Expected: compiles; run exits 0 with no crash artifact.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp3.rs fuzz/fuzz_targets/mp3.rs
git commit -m "test(fuzz): fuzz mp3 bounded prober against full parse + pin equivalence (#212)"
```

### Task 5: flac bounded oracle

**Files:**
- Modify: `fuzz/fuzz_targets/flac.rs`

- [ ] **Step 1: Add the bounded oracle**

In `fuzz/fuzz_targets/flac.rs`, change the imports line:

```rust
use musefs_format::{flac, fuzz_check::assert_backing_covers_audio};
```
to:
```rust
use musefs_format::{flac, fuzz_check::assert_backing_covers_audio, Extent};
```

Then, immediately after the block that binds `scan`:

```rust
    let scan = match flac::locate_audio(data) {
        Ok(s) => s,
        Err(_) => return,
    };
```

insert:

```rust
    // #212: flac's bounded twin takes no file_len, so NeedMore can fire at the
    // whole buffer when a block declares a body past EOF. Since locate_audio
    // succeeded here, the file fully parses, so the bounded twin must Complete
    // and equal read_metadata.
    match flac::read_metadata_bounded(data) {
        Ok(Extent::Complete(m)) => {
            let full = flac::read_metadata(data).expect("locate_audio Ok but read_metadata Err");
            assert_eq!(m, full, "flac bounded != full read_metadata");
        }
        other => panic!("flac bounded diverged (locate_audio succeeded): {other:?}"),
    }
```

- [ ] **Step 2: Build and short-run**

Run: `cargo +nightly fuzz build flac && cargo +nightly fuzz run flac -- -runs=5000 -max_total_time=20`
Expected: compiles; exits 0, no crash.

- [ ] **Step 3: Commit**

```bash
git add fuzz/fuzz_targets/flac.rs
git commit -m "test(fuzz): fuzz flac bounded prober against read_metadata (#212)"
```

### Task 6: wav bounded + ceiling oracle

**Files:**
- Modify: `fuzz/fuzz_targets/wav.rs`

- [ ] **Step 1: Add the bounded + ceiling oracle**

In `fuzz/fuzz_targets/wav.rs`, change the imports line:

```rust
use musefs_format::{fuzz_check::assert_backing_covers_audio, wav};
```
to:
```rust
use musefs_format::{fuzz_check::assert_backing_covers_audio, wav, Extent};
```

Then, immediately after the block that binds `bounds`:

```rust
    let bounds = match wav::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
```

insert:

```rust
    // #212: bounded twin on a whole buffer is literally Complete(locate_audio).
    let len = data.len() as u64;
    match wav::locate_audio_bounded(data, len) {
        Ok(Extent::Complete(b)) => assert_eq!(b, bounds, "wav bounded != full"),
        other => panic!("wav bounded diverged (whole buffer): {other:?}"),
    }
    // The ceiling prober trusts a declared `data` length validated against
    // file_len, so it may accept where locate_audio rejects. Assert only that it
    // stays in bounds (no equality oracle).
    if let Ok(c) = wav::locate_audio_at_ceiling(data, len) {
        assert!(
            c.audio_offset.saturating_add(c.audio_length) <= len,
            "wav ceiling region exceeds file_len: off={} len={} file_len={}",
            c.audio_offset,
            c.audio_length,
            len,
        );
    }
```

- [ ] **Step 2: Build and short-run**

Run: `cargo +nightly fuzz build wav && cargo +nightly fuzz run wav -- -runs=5000 -max_total_time=20`
Expected: compiles; exits 0, no crash.

- [ ] **Step 3: Commit**

```bash
git add fuzz/fuzz_targets/wav.rs
git commit -m "test(fuzz): fuzz wav bounded + ceiling probers (#212)"
```

### Task 7: ogg bounded oracle

**Files:**
- Modify: `fuzz/fuzz_targets/ogg.rs`

- [ ] **Step 1: Add the bounded oracle**

In `fuzz/fuzz_targets/ogg.rs`, change the imports line:

```rust
use musefs_format::{fuzz_check::assert_backing_covers_audio, ogg, ArtInput};
```
to:
```rust
use musefs_format::{fuzz_check::assert_backing_covers_audio, ogg, ArtInput, Extent};
```

Then, immediately after the block that binds `header`:

```rust
    let header = match ogg::read_metadata(data) {
        Ok(h) => h,
        Err(_) => return,
    };
```

insert:

```rust
    // #212: ogg::read_metadata == read_header; the bounded twin Completes only
    // when read_header succeeds, and cannot return NeedMore at a whole buffer.
    let len = data.len() as u64;
    match ogg::read_metadata_bounded(data, len) {
        Ok(Extent::Complete(h)) => assert_eq!(h, header, "ogg bounded != read_metadata"),
        Ok(Extent::NeedMore { up_to }) => {
            panic!("ogg bounded NeedMore at whole buffer: up_to={up_to}")
        }
        Err(_) => panic!("ogg bounded Err but read_metadata succeeded"),
    }
```

- [ ] **Step 2: Build and short-run**

Run: `cargo +nightly fuzz build ogg && cargo +nightly fuzz run ogg -- -runs=5000 -max_total_time=20`
Expected: compiles; exits 0, no crash.

- [ ] **Step 3: Commit**

```bash
git add fuzz/fuzz_targets/ogg.rs
git commit -m "test(fuzz): fuzz ogg bounded prober against read_metadata (#212)"
```

### Task 8: mp4 seeking-prober oracle

**Files:**
- Modify: `fuzz/fuzz_targets/mp4.rs`

- [ ] **Step 1: Add the read_structure_from oracle**

In `fuzz/fuzz_targets/mp4.rs`, after the block that binds `scan`:

```rust
    let scan = match mp4::read_structure(data) {
        Ok(s) => s,
        Err(_) => return,
    };
```

insert:

```rust
    // #212: the seeking variant reads headers and skips the mdat payload; on a
    // whole buffer it must produce the same Mp4Scan as the full-buffer parse.
    let mut cursor = std::io::Cursor::new(data);
    match mp4::read_structure_from(&mut cursor, data.len() as u64) {
        Ok(s) => assert_eq!(s, scan, "mp4 read_structure_from != read_structure"),
        Err(e) => panic!("mp4 read_structure_from Err but read_structure Ok: {e:?}"),
    }
```

(No `Extent` import needed — `read_structure_from` returns a plain `Result`.)

- [ ] **Step 2: Build and short-run**

Run: `cargo +nightly fuzz build mp4 && cargo +nightly fuzz run mp4 -- -runs=5000 -max_total_time=20`
Expected: compiles; exits 0, no crash.

- [ ] **Step 3: Commit**

```bash
git add fuzz/fuzz_targets/mp4.rs
git commit -m "test(fuzz): fuzz mp4 seeking prober against read_structure (#212)"
```

---

## Workstream C — #213 serve fuzzing + Ogg proptest

### Task 9: Ogg read-fidelity proptest

**Files:**
- Modify: `musefs-core/tests/proptest_read_fidelity.rs` (add builders after the m4a block near line 600; add a `proptest!` block after the existing m4a `proptest!` block)

- [ ] **Step 1: Add the `build_ogg` and `build_ogg_with_art` builders**

In `musefs-core/tests/proptest_read_fidelity.rs`, after the `build_m4a_with_art` function (around line 620, before the m4a `proptest!` block), add. (This mirrors `build`/`build_with_art` exactly, swapping the writer + format. `common::write_ogg` is already a sibling helper.)

```rust
/// Like `build`, but writes an Ogg (Opus) backing file and registers it as
/// `Format::Opus`. The Ogg serve path renumbers page headers, so served audio is
/// NOT byte-identical to the source — only splice consistency is asserted below.
fn build_ogg(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.opus");
    let (audio_offset, audio_length) = common::write_ogg(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format: Format::Opus,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime: i64::try_from(
                meta.modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap(),
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    (dir, db, id, audio.to_vec())
}

/// Like `build_ogg`, but links a DB art blob so Opus synthesis emits an
/// `OggArtSlice` (incremental base64) segment — exercising that serve path.
fn build_ogg_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.opus");
    let (audio_offset, audio_length) = common::write_ogg(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format: Format::Opus,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime: i64::try_from(
                meta.modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap(),
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: Some(8),
            height: Some(8),
            data: art.to_vec(),
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: "front".to_string(),
            ordinal: 0,
        }],
    )
    .unwrap();
    (dir, db, id)
}
```

- [ ] **Step 2: Add the failing Ogg proptest block**

After the existing m4a `proptest! { ... }` block (the last one in the file), add a new block. These properties assert splice consistency only — never `served == original` — because Ogg renumbers page headers.

```rust
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn ogg_read_at_partial_windows_match_whole(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build_ogg(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        prop_assert_eq!(whole.len() as u64, total);
        let offset = (a as u64) % (total + 1);
        let len = (b as u64) % (total - offset + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(got.len() as u64, len);
        prop_assert_eq!(&got[..], &whole[usize::try_from(offset).unwrap()..usize::try_from(offset + len).unwrap()]);
    }

    #[test]
    fn ogg_read_at_windows_spanning_header_seam(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        before in 0usize..4096,
        after in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build_ogg(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let hlen = resolved.layout.header_len();
        prop_assume!(hlen > 0 && hlen < total);
        let start = hlen - 1 - (before as u64 % hlen);
        let end = hlen + 1 + (after as u64 % (total - hlen));
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let got = read_at(&resolved, &db, start, end - start).unwrap();
        prop_assert_eq!(&got[..], &whole[usize::try_from(start).unwrap()..usize::try_from(end).unwrap()]);
    }

    #[test]
    fn ogg_with_art_partial_windows_match_whole(
        audio in proptest::collection::vec(any::<u8>(), 1..256),
        art in proptest::collection::vec(any::<u8>(), 1..256),
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id) = build_ogg_with_art(&audio, "T", &art);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let offset = (a as u64) % (total + 1);
        let len = (b as u64) % (total - offset + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(&got[..], &whole[usize::try_from(offset).unwrap()..usize::try_from(offset + len).unwrap()]);
    }
}
```

- [ ] **Step 3: Run the new proptests to verify they pass**

Run: `cargo test -p musefs-core --test proptest_read_fidelity ogg_ -- --nocapture`
Expected: PASS for all three `ogg_*` properties. If `build_ogg_with_art` panics on `resolve` because Opus art does not produce a servable layout, STOP and inspect the resolved layout's segments — the splice-consistency assertion does not depend on the segment *type*, so the property should still hold for whatever segments synthesis emits; a panic indicates a synthesis/resolve error to investigate, not a property bug.

- [ ] **Step 4: Run the full file to confirm no regression in the existing properties**

Run: `cargo test -p musefs-core --test proptest_read_fidelity`
Expected: all properties PASS (FLAC/WAV/MP3/M4A/OGG).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/proptest_read_fidelity.rs
git commit -m "test(core): add Ogg serve-path read-fidelity proptest (#213)"
```

### Task 10: serve fuzz target — crate wiring + skeleton

**Files:**
- Modify: `fuzz/Cargo.toml`
- Create: `fuzz/fuzz_targets/serve.rs`

- [ ] **Step 1: Add dependencies and the `[[bin]]` to `fuzz/Cargo.toml`**

In `fuzz/Cargo.toml`, under `[dependencies]`, after the `musefs-format` line, add:

```toml
musefs-core = { path = "../musefs-core" }
musefs-db = { path = "../musefs-db" }
tempfile = "3"
```

And after the last `[[bin]]` block (the `vorbiscomment` one), before the trailing `[workspace]`, add:

```toml
[[bin]]
name = "serve"
path = "fuzz_targets/serve.rs"
test = false
doc = false
bench = false
```

- [ ] **Step 2: Create a minimal `serve.rs` skeleton (Opus only) that resolves + reads the whole file**

Create `fuzz/fuzz_targets/serve.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_core::{read_at_with_file, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_fuzz::MAX_INPUT;
use std::io::Write;

/// Build a one-track in-memory DB over `backing` written to a temp file, and
/// return (tempdir, db, track_id, total backing len). Returns None on any setup
/// error (e.g. the fixture didn't parse for this format).
fn setup(
    backing: &[u8],
    format: Format,
    audio_offset: u64,
    audio_length: u64,
) -> Option<(tempfile::TempDir, Db, i64)> {
    let dir = tempfile::tempdir().ok()?;
    let path = dir.path().join("backing");
    std::fs::File::create(&path).ok()?.write_all(backing).ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    let db = Db::open_in_memory().ok()?;
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime: i64::try_from(
                meta.modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs(),
            )
            .ok()?,
        })
        .ok()?;
    db.replace_tags(id, &[Tag::new("title", "T", 0)]).ok()?;
    Some((dir, db, id))
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    // Skeleton: serve a fixed Opus fixture and read the whole virtual file.
    let backing = musefs_format::fuzz_check::fixtures::ogg_opus();
    let scan = match musefs_format::ogg::locate_audio(&backing) {
        Ok(s) => s,
        Err(_) => return,
    };
    let Some((_dir, db, id)) =
        setup(&backing, Format::Opus, scan.audio_offset, scan.audio_length)
    else {
        return;
    };
    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");
    let whole = read_at_with_file(&resolved, &db, &file, 0, resolved.total_len).unwrap();
    assert_eq!(whole.len() as u64, resolved.total_len, "whole read length != total_len");
});
```

- [ ] **Step 3: Build the target (proves the new dependency graph compiles)**

Run: `cargo +nightly fuzz build serve`
Expected: compiles. If it fails resolving `musefs-core`/`musefs-db` as a separate workspace, confirm the path deps are correct relative to `fuzz/` (`../musefs-core`, `../musefs-db`).

- [ ] **Step 4: Short-run the skeleton**

Run: `cargo +nightly fuzz run serve -- -runs=200 -max_total_time=15`
Expected: exits 0, no crash. (Throughput is low — Db + temp file per input — which is expected and why this target is scheduled-only.)

- [ ] **Step 5: Commit**

```bash
git add fuzz/Cargo.toml fuzz/Cargo.lock fuzz/fuzz_targets/serve.rs
git commit -m "test(fuzz): add serve-path target skeleton + musefs-core dep (#213)"
```

### Task 11: serve fuzz target — multi-format selector, windows, splice oracle

**Files:**
- Modify: `fuzz/fuzz_targets/serve.rs`

- [ ] **Step 1: Replace the skeleton `fuzz_target!` body with the full target**

Replace the entire `fuzz_target!(...)` block in `fuzz/fuzz_targets/serve.rs` with the following (keep the `setup` helper and the `use` lines; add `use arbitrary::Unstructured;` and the `arb_*` imports). The new imports block at the top becomes:

```rust
#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_core::{read_at_with_file, HeaderCache, Mode};
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};
use std::io::Write;
```

And the body:

```rust
fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT || data.is_empty() {
        return;
    }
    let mut u = Unstructured::new(data);

    // A fixed small audio payload keeps fixtures cheap and deterministic; the
    // adversarial surface is the selector, the DB tags/art, and the read windows.
    const AUDIO: &[u8] = &[1u8, 2, 3, 4, 5, 6, 7, 8];

    // Selector biases Opus so serve_ogg_window / OggArtSlice are well covered.
    let sel = u.int_in_range(0..=6u8).unwrap_or(6);
    let (backing, format, audio_offset, audio_length) = match sel {
        0 => {
            let b = musefs_format::fuzz_check::fixtures::flac(AUDIO);
            let s = match musefs_format::flac::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Flac, s.audio_offset, s.audio_length)
        }
        1 => {
            let b = musefs_format::fuzz_check::fixtures::wav(&[0i16, 1, -1, 100]);
            let s = match musefs_format::wav::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Wav, s.audio_offset, s.audio_length)
        }
        2 => {
            let b = musefs_format::fuzz_check::fixtures::mp3();
            let s = match musefs_format::mp3::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Mp3, s.audio_offset, s.audio_length)
        }
        3 => {
            let b = musefs_format::fuzz_check::fixtures::m4a(&[9u8; 32]);
            let s = match musefs_format::mp4::read_structure(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::M4a, s.mdat_payload_offset, s.mdat_payload_len)
        }
        _ => {
            let b = musefs_format::fuzz_check::fixtures::ogg_opus();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Opus, s.audio_offset, s.audio_length)
        }
    };

    let Some((_dir, db, id)) = setup(&backing, format, audio_offset, audio_length) else {
        return;
    };

    // Optionally attach fuzzer-chosen tags and a DB art blob (the art produces an
    // ArtImage / OggArtSlice segment, depending on format).
    let tags = arb_tags(&mut u).unwrap_or_default();
    if !tags.is_empty() {
        let db_tags: Vec<Tag> = tags
            .iter()
            .enumerate()
            .map(|(i, t)| Tag::new(&t.key, &t.value, i as u64))
            .collect();
        let _ = db.replace_tags(id, &db_tags);
    }
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Some(a) = arts.first() {
        let blob = vec![0xABu8; usize::try_from(a.data_len.min(4096)).unwrap_or(0)];
        if !blob.is_empty()
            && let Ok(art_id) = db.upsert_art(&NewArt {
                mime: a.mime.clone(),
                width: Some(8),
                height: Some(8),
                data: blob,
            })
        {
            let _ = db.set_track_art(
                id,
                &[TrackArt {
                    art_id,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 0,
                }],
            );
        }
    }

    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };
    let total = resolved.total_len;
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");

    // The single whole read every window is checked against (splice consistency).
    let whole = read_at_with_file(&resolved, &db, &file, 0, total).unwrap();
    assert_eq!(whole.len() as u64, total, "whole read length != total_len");

    // Draw up to 8 windows, including ranges that start at/after EOF or run past
    // it (offset/size range up to total+64). read_segments_into clamps the read
    // to [offset, total); an oversized/past-EOF range must not panic and must
    // return the clamped length (0 when offset >= total), and the bytes must
    // equal the clamped slice of the whole read.
    let slack = total.saturating_add(64);
    for _ in 0..8 {
        let offset = match u.int_in_range(0..=slack) {
            Ok(v) => v,
            Err(_) => break,
        };
        let size = match u.int_in_range(0..=slack) {
            Ok(v) => v,
            Err(_) => break,
        };
        let got = read_at_with_file(&resolved, &db, &file, offset, size).unwrap();
        // Mirror read_segments_into's clamp: served = [min(offset,total), min(offset+size,total)).
        let end = offset.saturating_add(size).min(total);
        let expected = end.saturating_sub(offset.min(total));
        assert_eq!(got.len() as u64, expected, "clamped window length mismatch");
        if expected > 0 {
            assert_eq!(
                got.as_slice(),
                &whole[usize::try_from(offset).unwrap()
                    ..usize::try_from(offset + expected).unwrap()],
                "window != clamped slice of whole read",
            );
        }
    }
});
```

Field reference (verified): `TagInput { pub key: String, pub value: String }`; `ArtInput { pub mime: String, pub data_len: u64, .. }`. `Tag::new(key: &str, value: &str, ordinal: u64)`. These match the accesses above.

- [ ] **Step 2: Build the target**

Run: `cargo +nightly fuzz build serve`
Expected: compiles. Fix any field-name mismatches against `fuzz/src/lib.rs` / `musefs-format` input structs if the compiler complains.

- [ ] **Step 3: Short-run across formats**

Run: `cargo +nightly fuzz run serve -- -runs=2000 -max_total_time=30`
Expected: exits 0, no crash artifact. The run should exercise multiple formats (selector spreads across the fixed fixtures).

- [ ] **Step 4: Commit**

```bash
git add fuzz/fuzz_targets/serve.rs
git commit -m "test(fuzz): serve-path target with multi-format selector + splice oracle (#213)"
```

### Task 12: serve target seeds

**Files:**
- Modify: `fuzz/src/bin/generate_seeds.rs`

- [ ] **Step 1: Add `serve` seeds**

In `fuzz/src/bin/generate_seeds.rs`, before the final `println!`, add seeds that decode to each covered selector value followed by a couple of window-spec bytes. The first byte is consumed by `u.int_in_range(0..=6)`; the remaining bytes feed `arb_tags`/`arb_arts` and the window draws. A few non-zero trailing bytes are enough to reach a real read:

```rust
    // serve target: one seed per covered format selector. Byte 0 picks the format
    // (int_in_range(0..=6)); the trailing bytes drive tags/art + read windows.
    for (name, sel) in [
        ("seed_flac", 0u8),
        ("seed_wav", 1u8),
        ("seed_mp3", 2u8),
        ("seed_m4a", 3u8),
        ("seed_opus", 6u8),
    ] {
        write("serve", name, &[sel, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
    }
```

- [ ] **Step 2: Regenerate and confirm the serve corpus exists**

Run:
```bash
( cd fuzz && cargo run --bin generate_seeds )
ls fuzz/corpus/serve/
```
Expected: lists `seed_flac seed_m4a seed_mp3 seed_opus seed_wav`. (Run from inside `fuzz/` — `fuzz` is excluded from the root workspace, so `cargo run -p musefs-fuzz` from the root fails.)

- [ ] **Step 3: Run serve over the new corpus**

Run: `cargo +nightly fuzz run serve -- -runs=0`
Expected: replays the 5 seeds once each, exits 0, no crash. (`-runs=0` over the corpus is a deterministic single pass.)

- [ ] **Step 4: Commit**

```bash
git add fuzz/src/bin/generate_seeds.rs fuzz/corpus/serve
git commit -m "test(fuzz): seed the serve target with one input per format (#213)"
```

---

## Workstream A — #216 regression durability

### Task 13: regressions directory + deterministic replay job + docs

**Files:**
- Create: `fuzz/regressions/<target>/.gitkeep` for each target
- Modify: `.github/workflows/fuzz.yml`
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Create the regressions directories with `.gitkeep`**

Run:
```bash
for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment serve; do
  mkdir -p "fuzz/regressions/$t"
  : > "fuzz/regressions/$t/.gitkeep"
done
ls -d fuzz/regressions/*/
```
Expected: nine directories listed, each containing `.gitkeep`.

- [ ] **Step 2: Verify the replay loop logic locally (no reproducers yet → all skipped)**

Run this exact loop body (the one the CI job will use) to confirm empty dirs are skipped without error:
```bash
shopt -s nullglob
for dir in fuzz/regressions/*/; do
  target=$(basename "$dir")
  files=("$dir"*)
  if [ ${#files[@]} -eq 0 ]; then
    echo "skip $target (no reproducers)"
    continue
  fi
  echo "would replay $target: ${files[*]}"
done
```
Expected: prints `skip <target> (no reproducers)` for every target (the `.gitkeep` is a dotfile and `*` does not match it, so `files` is empty). No errors.

- [ ] **Step 3: Add the deterministic replay as a step in the existing `smoke` job**

The `smoke` job already does `cargo install cargo-fuzz` + `cargo +nightly fuzz build` (which now also builds `serve`, pulling in `musefs-core`). Reuse that build: add the replay as a step in `smoke` **right after the "Build targets" step and before the "Smoke-run each target" step**, so the fast deterministic regression check runs first and a second `musefs-core`-dependent build is avoided. In `.github/workflows/fuzz.yml`, in the `smoke` job's `steps:`, insert:

```yaml
      - name: Replay committed reproducers (-runs=0)
        run: |
          shopt -s nullglob
          status=0
          for dir in fuzz/regressions/*/; do
            target=$(basename "$dir")
            files=("$dir"*)
            if [ ${#files[@]} -eq 0 ]; then
              echo "== $target: no reproducers, skipping =="
              continue
            fi
            echo "== $target: replaying ${#files[@]} reproducer(s) =="
            cargo +nightly fuzz run "$target" "${files[@]}" -- -runs=0 || status=1
          done
          exit $status
```

So the `smoke` job step order becomes: checkout → toolchain → rust-cache → Install cargo-fuzz → Build targets → **Replay committed reproducers (-runs=0)** → Smoke-run each target. The replay is not time-boxed (no `-max_total_time`); it deterministically replays each committed reproducer once and fails the job if any panics.

- [ ] **Step 4: Add `serve` to the `scheduled` matrix only (NOT the smoke loop)**

In `.github/workflows/fuzz.yml`, in the `scheduled` job's `matrix.target` list, add `serve`:

```yaml
        target: [flac, mp3, mp4, ogg, wav, ogg_page, b64, vorbiscomment, serve]
```

Leave the `smoke` job's `for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment` loop **unchanged** — `serve` is built by `cargo +nightly fuzz build` but is too low-throughput for the 15s smoke window.

- [ ] **Step 5: Lint the workflow YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/fuzz.yml')); print('yaml ok')"`
Expected: prints `yaml ok`.

- [ ] **Step 6: Document the convention in `CONTRIBUTING.md`**

Find the fuzzing section in `CONTRIBUTING.md` (search for `cargo +nightly fuzz` or the coverage-guided-fuzzing anchor) and add a subsection. Use this text:

```markdown
### Fuzz crash regressions

When you fix a fuzz-found crash:

1. Drop the reproducer bytes into `fuzz/regressions/<target>/` (one file per
   reproducer). The per-PR fuzz `smoke` job's replay step runs every committed
   reproducer with `cargo +nightly fuzz run <target> <files> -- -runs=0` — a
   deterministic single pass that fails the build if any known input panics
   again. This is separate from `fuzz/corpus/`, which `cargo fuzz cmin`
   minimizes (and would prune reproducers from).
2. Where the crash exposed a real logic/behavior defect, also add a focused
   behavioral test for that logic in the owning crate's suite (the pre-commit
   hook gates it). The byte replay proves the exact input no longer panics; the
   behavioral test documents and locks in the fix. They are not interchangeable.

Coverage notes: the per-format targets also drive the bounded/ceiling probers
(`*_bounded`, `locate_audio_at_ceiling`, `read_structure_from`) and assert a
differential oracle against the full-buffer parse. The `serve` target fuzzes the
read-time serve path (`read_at_with_file` over adversarial layouts, including
`serve_ogg_window`/`OggArtSlice`) and is scheduled-only (built per-PR, not
smoke-run) because it builds a DB + temp backing file per input.
```

- [ ] **Step 7: Commit**

```bash
git add fuzz/regressions .github/workflows/fuzz.yml CONTRIBUTING.md
git commit -m "ci(fuzz): deterministic regression replay + serve in scheduled matrix (#216)"
```

---

## Final verification

- [ ] **Step 1: Full workspace test + lint (pre-commit parity)**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: all green. (This is what the pre-commit hook runs; every commit above should already have passed it for the workspace-touching tasks.)

- [ ] **Step 2: Build every fuzz target (out-of-workspace gate)**

Run: `cargo +nightly fuzz build`
Expected: all targets including `serve` compile. (CI's smoke + regressions jobs both run this; it is the only gate that catches fuzz-crate breakage from format/core API changes.)

- [ ] **Step 3: Deterministic corpus replay across all targets**

Run:
```bash
for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment serve; do
  echo "== $t ==" && cargo +nightly fuzz run "$t" -- -runs=0
done
```
Expected: each replays its committed corpus once and exits 0, no crash artifacts.

---

## Spec coverage check

- #216 → Task 13 (regressions dir + `-runs=0` replay job + CONTRIBUTING convention).
- #212 → Tasks 4–8 (mp3 equivalence test + per-format bounded oracles, per the spec's per-format table).
- #213 → Tasks 9 (Ogg proptest), 10–12 (serve target + deps + seeds, `read_at_with_file`, scheduled-only).
- #197 → Tasks 1 (b64 assert), 2 (bench reorder), 3 (mp3 binary-frame fixture + seed).
