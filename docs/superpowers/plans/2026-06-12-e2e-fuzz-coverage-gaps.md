# E2E/Fuzz Coverage Gaps (#320, #306, #313) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close three independent test/fuzz coverage gaps from the #280 audit: fix and CI-wire the stale `musefs`/`musefs-latencyfs` e2e tests (#320), expand read-only refusal coverage (#306), and expand `serve` fuzzing with hostile DB rows, binary-tag streaming, and distinct Ogg fixtures (#313).

**Architecture:** All changes are test/fuzz-only except one additive, feature-gated DB accessor (`Db::with_raw_conn`, behind a new off-by-default `fuzzing` feature consumed only by the out-of-workspace fuzz crate). No production behavior changes. The cardinal invariant (audio bytes never copied/modified) is untouched.

**Tech Stack:** Rust, FUSE (fuser), SQLite (rusqlite), cargo-fuzz (libfuzzer), raw `libc` syscalls for the read-only probes.

**Spec:** `docs/superpowers/specs/2026-06-12-e2e-fuzz-coverage-gaps-design.md`

**Environment note:** Tasks 1 and 2 produce `#[ignore]` e2e tests that require `/dev/fuse` + `fusermount3`. This host has them — run with `--ignored`. The pre-commit hook runs the full workspace test suite (excludes `--ignored`), so each commit is green regardless. The fuzz crate (`fuzz/`) is **outside** the workspace; verify it with `cargo +nightly fuzz build <target>`, never plain `cargo build`.

---

## File Structure

- `musefs/tests/sigterm_unmount.rs` — (modify) add explicit `--template` to both mounts. *(Task 1)*
- `.github/workflows/ci.yml` — (modify) wire `musefs` + `musefs-latencyfs` `--ignored` into the `e2e` job. *(Task 1)*
- `musefs-fuse/tests/read_consistency.rs` — (modify) extend the read-only refusal test + add `assert_read_safe`. *(Task 2)*
- `musefs-db/Cargo.toml` — (modify) add the `fuzzing` feature. *(Task 3)*
- `musefs-db/src/lib.rs` — (modify) add `#[cfg(feature = "fuzzing")] Db::with_raw_conn`. *(Task 3)*
- `musefs-format/src/fuzz_check.rs` — (modify) add `ogg_vorbis()` / `ogg_flac()` fixtures + recognition tests. *(Task 4)*
- `fuzz/Cargo.toml` — (modify) enable `musefs-db/fuzzing`, add `rusqlite`. *(Task 5)*
- `fuzz/fuzz_targets/serve.rs` — (modify) distinct Ogg selectors, binary-tag opt-in, hostile-row stage, assertion discipline. *(Task 5)*
- `CONTRIBUTING.md` — (modify) document the new fuzz scope + CI e2e steps. *(Task 6)*

---

## Task 1: #320 — Fix stale sigterm tests + wire binary e2e into CI

**Files:**
- Modify: `musefs/tests/sigterm_unmount.rs:96` and `:142`
- Modify: `.github/workflows/ci.yml` (the `e2e` job, after the existing `musefs-fuse` steps near `:304-306`)

- [ ] **Step 1: Reproduce the failure (test is red on this `/dev/fuse` host)**

Run: `cargo test -p musefs --test sigterm_unmount -- --ignored --test-threads=1`
Expected: `FAILED. 0 passed; 2 failed`, both panicking `mount did not come up` (the fixture renders `Unknown/Unknown/Song.flac`, not the asserted `Alice/Song.flac`).

- [ ] **Step 2: Add an explicit template to the first mount**

In `sigterm_unmounts_cleanly`, change the mount invocation (currently at `:95-98`):

```rust
    let mut child = Command::new(bin)
        .args(["mount", mp.path().to_str().unwrap(), "--db", db])
        .spawn()
        .unwrap();
```

to:

```rust
    let mut child = Command::new(bin)
        .args([
            "mount",
            mp.path().to_str().unwrap(),
            "--db",
            db,
            "--template",
            "$artist/$title",
        ])
        .spawn()
        .unwrap();
```

- [ ] **Step 3: Add the same explicit template to the second mount**

In `sigterm_exits_bounded_when_mount_is_busy`, apply the identical change to the mount invocation (currently at `:141-143`):

```rust
    let mut child = Command::new(bin)
        .args([
            "mount",
            mp.path().to_str().unwrap(),
            "--db",
            db,
            "--template",
            "$artist/$title",
        ])
        .spawn()
        .unwrap();
```

(The fixtures already tag `ARTIST=Alice` / `TITLE=Song`, so `$artist/$title` renders the asserted `Alice/Song.flac`. No fixture or assertion-path change.)

- [ ] **Step 4: Run the sigterm tests — expect green**

Run: `cargo test -p musefs --test sigterm_unmount -- --ignored --test-threads=1`
Expected: `2 passed; 0 failed`.

- [ ] **Step 5: Verify the latencyfs ignored e2e is green BEFORE wiring it (contingency)**

Run: `cargo test -p musefs-latencyfs -- --ignored --test-threads=1`
Expected: all pass. **If any fail:** do NOT proceed to wire latencyfs. latencyfs is a read-write passthrough with no synthesis template, so a failure is passthrough/latency-behavior drift, **not** virtual-path drift — do not "fix" it with a template tweak. Stop, wire only `musefs` in Step 6, and report the latencyfs failure as a separate finding.

- [ ] **Step 6: Wire both crates' ignored e2e into CI**

In `.github/workflows/ci.yml`, the `e2e` job currently ends with (near `:304-306`):

```yaml
      - name: FUSE end-to-end tests
        run: cargo test -p musefs-fuse -- --ignored
      - name: FUSE fault-injection + concurrency (metrics feature)
        run: cargo test -p musefs-fuse --features metrics -- --ignored
```

Add two steps immediately after the second one:

```yaml
      - name: Binary-level end-to-end tests (musefs)
        run: cargo test -p musefs -- --ignored
      - name: latencyfs end-to-end tests
        run: cargo test -p musefs-latencyfs -- --ignored
```

(They inherit the job's `if: needs.changes.outputs.src == 'true'` gate — they run on `src`-touching PRs, matching the existing `musefs-fuse` steps.)

- [ ] **Step 7: Lint the workflow file**

Run: `yamllint .github/workflows/ci.yml`
Expected: no errors (the pre-commit hook also runs this).

- [ ] **Step 8: Commit**

```bash
git add musefs/tests/sigterm_unmount.rs .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
test(musefs): pin sigterm e2e to explicit template; wire binary e2e into CI (#320)

Both sigterm tests now mount with --template '$artist/$title' so the
ARTIST/TITLE-only fixture renders the asserted Alice/Song.flac regardless
of the default template. Wire musefs + musefs-latencyfs --ignored e2e into
the CI e2e job so they cannot silently rot again.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: #306 — Expand read-only refusal coverage

**Files:**
- Modify: `musefs-fuse/tests/read_consistency.rs` (the `assert_refused` helper region `:273-285`, and the `write_ops_are_refused_on_read_only_mount` test `:287-356`)

The existing test mounts a single FLAC at `mountpoint/Alice/Song.flac` (`with_single_flac_mount`, `:85-99`). `served` is that path; `mountpoint` is the mount root; `Alice` is a virtual directory. Existing helpers: `cstr(&Path) -> CString` (`:265`), `last_errno() -> i32` (`:269`), `assert_refused(ret: i32, accepted: &[i32], what: &str)` (`:275`).

- [ ] **Step 1: Reproduce current pass (baseline)**

Run: `cargo test -p musefs-fuse --test read_consistency write_ops_are_refused_on_read_only_mount -- --ignored`
Expected: `1 passed` (we are adding coverage to a passing test).

- [ ] **Step 2: Add the `assert_read_safe` helper**

Immediately after `assert_refused` (after `:285`), add:

```rust
/// Assert a libc *read* call (getxattr/listxattr) did not error as if it were a
/// write: it either succeeds (`ret >= 0`) or fails with a non-mutation errno in
/// `accepted`. Refusing a read on a read-only mount would itself be the bug.
fn assert_read_safe(ret: isize, accepted: &[i32], what: &str) {
    if ret >= 0 {
        return;
    }
    let e = last_errno();
    assert!(
        accepted.contains(&e),
        "{what}: errno {e} not in accepted read-safe set {accepted:?}"
    );
}
```

- [ ] **Step 3: Extend the test body with the new mutating + read probes**

In `write_ops_are_refused_on_read_only_mount`, the `new_dir` binding currently exists (`:298`) and `new_file` (`:297`). Add a `rename`/`link`/`symlink` destination and a directory target. Replace the tail of the `unsafe { … }` block — the part from the `utimes` call (`:349-353`) to the block's close `}` (`:354`) — so it reads:

```rust
            assert_refused(
                libc::utimes(existing.as_ptr(), times.as_ptr()),
                &[libc::EROFS, libc::EPERM, libc::EACCES],
                "utimes",
            );

            // --- #306: additional mutating-syscall families ---
            let renamed = cstr(&mountpoint.join("Alice").join("renamed.flac"));
            let link_dst = cstr(&mountpoint.join("Alice").join("hardlink.flac"));
            let sym_dst = cstr(&mountpoint.join("Alice").join("symlink.flac"));
            let dir = cstr(&mountpoint.join("Alice"));

            assert_refused(
                libc::rename(existing.as_ptr(), renamed.as_ptr()),
                &[libc::EROFS, libc::EPERM, libc::EACCES],
                "rename",
            );
            assert_refused(
                libc::rmdir(dir.as_ptr()),
                &[libc::EROFS, libc::EPERM, libc::EACCES, libc::ENOTEMPTY],
                "rmdir",
            );
            assert_refused(
                libc::symlink(existing.as_ptr(), sym_dst.as_ptr()),
                &[libc::EROFS, libc::EPERM, libc::EACCES],
                "symlink",
            );
            assert_refused(
                libc::chown(existing.as_ptr(), 0, 0),
                &[libc::EROFS, libc::EPERM, libc::EACCES],
                "chown",
            );
            assert_refused(
                libc::lchown(existing.as_ptr(), 0, 0),
                &[libc::EROFS, libc::EPERM, libc::EACCES],
                "lchown",
            );
            // mknod a regular file (no privilege needed); ENOSYS tolerates a
            // platform/FUSE build that does not implement the callback at all.
            assert_refused(
                libc::mknod(new_file.as_ptr(), libc::S_IFREG | 0o644, 0),
                &[libc::EROFS, libc::EPERM, libc::EACCES, libc::ENOSYS],
                "mknod",
            );
            assert_refused(
                libc::link(existing.as_ptr(), link_dst.as_ptr()),
                &[libc::EROFS, libc::EPERM, libc::EACCES, libc::ENOSYS],
                "link",
            );

            // xattr syscalls differ in signature across platforms (Linux vs
            // macOS); this test only ever *runs* on Linux (/dev/fuse), so gate
            // the xattr probes so the macOS workspace build still compiles.
            #[cfg(target_os = "linux")]
            {
                let xval = [0u8; 4];
                assert_refused(
                    libc::setxattr(
                        existing.as_ptr(),
                        c"user.test".as_ptr(),
                        xval.as_ptr().cast(),
                        xval.len(),
                        0,
                    ),
                    &[libc::EROFS, libc::EPERM, libc::EACCES, libc::ENOTSUP],
                    "setxattr",
                );
                assert_refused(
                    libc::removexattr(existing.as_ptr(), c"user.test".as_ptr()),
                    &[
                        libc::EROFS,
                        libc::EPERM,
                        libc::EACCES,
                        libc::ENOTSUP,
                        libc::ENODATA,
                    ],
                    "removexattr",
                );

                // Read probes: NOT mutations. Assert read-safety, never refusal.
                let mut buf = [0u8; 256];
                assert_read_safe(
                    libc::getxattr(
                        existing.as_ptr(),
                        c"user.test".as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    ),
                    &[libc::ENOTSUP, libc::ENODATA],
                    "getxattr",
                );
                assert_read_safe(
                    libc::listxattr(existing.as_ptr(), buf.as_mut_ptr().cast(), buf.len()),
                    &[libc::ENOTSUP],
                    "listxattr",
                );
            }
        }
```

- [ ] **Step 4: Run the extended test — expect green on `/dev/fuse`**

Run: `cargo test -p musefs-fuse --test read_consistency write_ops_are_refused_on_read_only_mount -- --ignored`
Expected: `1 passed`. If a mutating syscall *succeeds*, that is a real read-only-contract regression — stop and investigate, do not loosen the assertion.

- [ ] **Step 5: Confirm the workspace still compiles (non-Linux compile guard)**

Run: `cargo test -p musefs-fuse --no-run`
Expected: compiles clean (verifies the `#[cfg(target_os = "linux")]` gating and the new helper).

- [ ] **Step 6: Commit**

```bash
git add musefs-fuse/tests/read_consistency.rs
git commit -m "$(cat <<'EOF'
test(fuse): cover rename/rmdir/symlink/chown/xattr/mknod/link refusal (#306)

Extend write_ops_are_refused_on_read_only_mount with the remaining mutating
syscall families (refused, broad errno sets) and exercise getxattr/listxattr
as read-safe (success or ENOTSUP/ENODATA), not refused. xattr probes gated to
Linux so the macOS workspace build still compiles.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: #313a — Feature-gated raw DB accessor

**Files:**
- Modify: `musefs-db/Cargo.toml` (`[features]`)
- Modify: `musefs-db/src/lib.rs` (the `impl<M> Db<M>` block at `:128-145`; add a feature-gated test module at end of file)

- [ ] **Step 1: Add the `fuzzing` feature**

In `musefs-db/Cargo.toml`, the `[features]` section currently is:

```toml
[features]
# Test-only: gates `Default` impls (Db + model structs) so cargo-mutants'
# `Ok(Default::default())` mutants compile. Named after the activity that needs
# it, mirroring musefs-format's `fuzzing` feature. Not for production use.
mutants = []
```

Add below `mutants = []`:

```toml
# Test/fuzz-only: exposes `Db::with_raw_conn` so the out-of-workspace fuzz crate
# can plant hostile rows the validating public API cannot produce. Never enabled
# in normal builds. Mirrors the `mutants` feature pattern.
fuzzing = []
```

- [ ] **Step 2: Add the `with_raw_conn` accessor**

In `musefs-db/src/lib.rs`, inside the `impl<M> Db<M>` block, after the `path()` method (which ends at `:144`), add:

```rust
    /// TEST/FUZZ ONLY (the `fuzzing` feature). Hands the raw rusqlite connection
    /// to `f` so fuzz harnesses can plant rows the validating public API cannot
    /// produce — e.g. negative geometry under `PRAGMA ignore_check_constraints`,
    /// or orphaned `track_art` under `PRAGMA foreign_keys = OFF`. Never compiled
    /// in production: the `fuzzing` feature is enabled only by the out-of-workspace
    /// fuzz crate.
    #[cfg(feature = "fuzzing")]
    pub fn with_raw_conn<R>(&self, f: impl FnOnce(&rusqlite::Connection) -> R) -> R {
        f(&self.conn)
    }
```

- [ ] **Step 3: Add a feature-gated round-trip test**

At the end of `musefs-db/src/lib.rs`, add:

```rust
#[cfg(all(test, feature = "fuzzing"))]
mod fuzzing_accessor_tests {
    use super::*;
    use crate::models::NewTrack;

    #[test]
    fn with_raw_conn_plants_a_constraint_violating_row() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x".to_string(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();

        db.with_raw_conn(|conn| {
            conn.execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            conn.execute(
                "UPDATE tracks SET audio_offset = -1 WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        });

        let off: i64 = db.with_raw_conn(|conn| {
            conn.query_row(
                "SELECT audio_offset FROM tracks WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(off, -1, "ignore_check_constraints let the negative offset land");
    }
}
```

- [ ] **Step 4: Verify the feature-on build + test**

Run: `cargo test -p musefs-db --features fuzzing fuzzing_accessor_tests`
Expected: `1 passed`.

- [ ] **Step 5: Verify the feature-off workspace build is unaffected**

Run: `cargo test -p musefs-db`
Expected: passes; `fuzzing_accessor_tests` and `with_raw_conn` are not compiled (no errors, no warnings).

- [ ] **Step 6: Commit**

```bash
git add musefs-db/Cargo.toml musefs-db/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(db): add fuzzing-gated Db::with_raw_conn for hostile-row injection (#313)

Off-by-default `fuzzing` feature (mirrors the `mutants` pattern) exposes the
raw rusqlite connection so the out-of-workspace fuzz crate can plant rows the
validating public API cannot produce. Not compiled in normal builds.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: #313b — Distinct Ogg Vorbis / OggFLAC fixtures

**Files:**
- Modify: `musefs-format/src/fuzz_check.rs` (the `fixtures` module, after `ogg_opus()` at `:251`; the `fixtures_tests` module at `:408`)

`fuzz_check` compiles under `#[cfg(any(test, feature = "fuzzing"))]`, so `cargo test -p musefs-format` runs the recognition tests — a real red/green loop. The Ogg page helpers are `crate::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty}`. `detect_codec` keys on the first packet's magic (`\x01vorbis`, `\x7FFLAC`); `read_header` reads 3 header packets for Vorbis and `1 + count` for OggFLAC (`count` = big-endian `u16` at mapping-header bytes 7..9).

**Clean-path synthesis is safe by construction** (relevant once Task 5 feeds these through the full resolve+read path): Ogg synthesis clones the identification/setup/mapping packets verbatim and **rebuilds the comment packet from the DB `tags` table** — it never parses the fixture's VorbisComment body or STREAMINFO. So the deliberately-omitted framing bit and the all-zero STREAMINFO are never read on the synthesis path; a fixture that `read_header` recognizes will synthesize cleanly. The recognition tests below are therefore a sufficient gate.

- [ ] **Step 1: Write the failing recognition tests**

In `musefs-format/src/fuzz_check.rs`, inside `mod fixtures_tests` (after the existing FLAC/m4a tests), add:

```rust
    #[test]
    fn ogg_vorbis_fixture_is_recognized() {
        let f = fixtures::ogg_vorbis();
        let h = crate::ogg::read_header(&f).unwrap();
        assert_eq!(h.codec, crate::ogg::Codec::Vorbis);
        let scan = crate::ogg::locate_audio(&f).unwrap();
        assert!(scan.audio_length > 0, "audio must be non-empty");
    }

    #[test]
    fn ogg_flac_fixture_is_recognized() {
        let f = fixtures::ogg_flac();
        let h = crate::ogg::read_header(&f).unwrap();
        assert_eq!(h.codec, crate::ogg::Codec::OggFlac);
        let scan = crate::ogg::locate_audio(&f).unwrap();
        assert!(scan.audio_length > 0, "audio must be non-empty");
    }
```

- [ ] **Step 2: Run them — expect failure (fixtures don't exist yet)**

Run: `cargo test -p musefs-format fixtures_tests::ogg_`
Expected: FAIL to compile (`no function ogg_vorbis in fixtures`).

- [ ] **Step 3: Add the `ogg_vorbis()` fixture**

In the `fixtures` module, immediately after `ogg_opus()` (after `:251`), add:

```rust
    /// Minimal Ogg **Vorbis**: 3 header packets (identification, comment, setup)
    /// then one audio packet. `detect_codec` keys on the `\x01vorbis` magic of
    /// packet 0; `read_header` reassembles exactly 3 Vorbis header packets. The
    /// comment packet carries an empty (but valid, round-trippable) VorbisComment
    /// body so synthesis can splice tags without erroring.
    pub fn ogg_vorbis() -> Vec<u8> {
        use crate::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty};
        let mut id = b"\x01vorbis".to_vec();
        id.extend_from_slice(&[0u8; 23]); // pad toward the 30-byte id-header shape
        let mut comment = b"\x03vorbis".to_vec();
        comment.extend_from_slice(&vorbis_body_empty());
        let setup = b"\x05vorbis".to_vec();
        let (mut data, pages) = build_header_pub(0x4321, &[&id, &comment, &setup]);
        let (audio, _) = lace_packet_pub(0x4321, pages, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);
        data
    }
```

- [ ] **Step 4: Add the `ogg_flac()` fixture**

Immediately after `ogg_vorbis()`, add:

```rust
    /// Minimal **OggFLAC**: mapping header packet 0
    /// (`0x7F "FLAC" major minor count(2 BE) "fLaC" STREAMINFO`) plus one
    /// following VORBIS_COMMENT metadata-block packet (`count = 1`), then one
    /// audio packet. `detect_codec` keys on `\x7FFLAC`; `read_header` reads
    /// `1 + count` header packets. Lengths are computed from the bodies so the
    /// 24-bit metadata-block length is always self-consistent.
    pub fn ogg_flac() -> Vec<u8> {
        use crate::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty};

        // STREAMINFO metadata block: header (type 0, not-last) + 24-bit len + body.
        let mut streaminfo = vec![0x00u8, 0x00, 0x00, 0x22]; // len 34
        streaminfo.extend_from_slice(&[0u8; 34]);

        let mut p0 = Vec::new();
        p0.push(0x7F);
        p0.extend_from_slice(b"FLAC");
        p0.push(1); // mapping major version
        p0.push(0); // mapping minor version
        p0.extend_from_slice(&1u16.to_be_bytes()); // count: 1 following packet
        p0.extend_from_slice(b"fLaC");
        p0.extend_from_slice(&streaminfo);

        // Following packet: VORBIS_COMMENT block (type 4, last-metadata-block set).
        let vc_body = vorbis_body_empty();
        let mut p1 = Vec::new();
        p1.push(0x84); // 0x80 last-flag | type 4
        let len = u32::try_from(vc_body.len()).expect("empty vorbis body fits in u24");
        p1.extend_from_slice(&len.to_be_bytes()[1..4]); // 24-bit big-endian length
        p1.extend_from_slice(&vc_body);

        let (mut data, pages) = build_header_pub(0x7777, &[&p0, &p1]);
        let (audio, _) = lace_packet_pub(0x7777, pages, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);
        data
    }
```

- [ ] **Step 5: Run the recognition tests — expect green**

Run: `cargo test -p musefs-format fixtures_tests::ogg_`
Expected: `2 passed` (`ogg_vorbis_fixture_is_recognized`, `ogg_flac_fixture_is_recognized`).

- [ ] **Step 6: Run the full format crate tests + fuzz build smoke**

Run: `cargo test -p musefs-format`
Expected: all pass.
Run: `cargo +nightly fuzz build serve`
Expected: builds (confirms the new fixtures compile under the `fuzzing` feature as the fuzz crate sees them).

- [ ] **Step 7: Commit**

```bash
git add musefs-format/src/fuzz_check.rs
git commit -m "$(cat <<'EOF'
test(format): add ogg_vorbis/ogg_flac fuzz fixtures (#313)

Distinct minimal Ogg Vorbis and OggFLAC fixtures (built from the existing
page_test_support helpers) so the serve fuzz target's Ogg selectors can feed
the shared Opus|Vorbis|OggFlac branch distinct inputs. Recognition tests pin
codec detection + audio location.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: #313c — Hostile rows, binary tags, distinct selectors in `serve.rs`

**Files:**
- Modify: `fuzz/Cargo.toml` (`[dependencies]`)
- Modify: `fuzz/fuzz_targets/serve.rs`

The fuzz crate is out-of-workspace, so these edits never touch the workspace test suite — verify exclusively with `cargo +nightly fuzz build serve` and a short `cargo +nightly fuzz run serve`.

- [ ] **Step 1: Enable the db `fuzzing` feature and add `rusqlite` to the fuzz crate**

In `fuzz/Cargo.toml`, the `[dependencies]` currently include:

```toml
musefs-db = { path = "../musefs-db" }
```

Change that line to enable the feature, and add `rusqlite` (same version/features as `musefs-db`, so the build unifies):

```toml
musefs-db = { path = "../musefs-db", features = ["fuzzing"] }
rusqlite = { version = "0.40", features = ["bundled"] }
```

- [ ] **Step 2: Update imports in `serve.rs`**

The current `use` block is:

```rust
use musefs_core::{HeaderCache, Mode, read_at_with_file};
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_tags};
```

Change the `musefs_db` line to add `BinaryTag`:

```rust
use musefs_db::{BinaryTag, Db, Format, NewArt, NewTrack, Tag, TrackArt};
```

- [ ] **Step 3: Split selectors 4/5/6 into Opus / Vorbis / OggFLAC**

In `serve.rs`, the selector `match sel { … }` currently has a single `_ =>` arm building `ogg_opus()` for 4–6. Replace that final `_ =>` arm with three explicit arms:

```rust
        4 => {
            let b = musefs_format::fuzz_check::fixtures::ogg_opus();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Opus, s.audio_offset, s.audio_length)
        }
        5 => {
            let b = musefs_format::fuzz_check::fixtures::ogg_vorbis();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::Vorbis, s.audio_offset, s.audio_length)
        }
        _ => {
            let b = musefs_format::fuzz_check::fixtures::ogg_flac();
            let s = match musefs_format::ogg::locate_audio(&b) {
                Ok(s) => s,
                Err(_) => return,
            };
            (b, Format::OggFlac, s.audio_offset, s.audio_length)
        }
```

- [ ] **Step 4: Add the binary-tag opt-in (after the existing art block)**

After the `arb_arts` / `set_track_art` block (the `if let Some(a) = arts.first() { … }`), and before the `let resolved = …` line, add:

```rust
    // Optionally attach DB binary tags so synthesis materializes a
    // Segment::BinaryTag and the read windows exercise read_binary_tag_chunk_into.
    // Only MP3 (4-byte frame id) and M4a (`----:<mean>:<name>` freeform atom)
    // synthesize opaque binary tags from the DB; for any other format the row is
    // silently dropped at synthesis, so restrict the opt-in to those two.
    if matches!(format, Format::Mp3 | Format::M4a) && u.arbitrary::<bool>().unwrap_or(false) {
        let key = match format {
            Format::Mp3 => "GEOB".to_string(),
            _ => "----:com.apple.iTunes:FUZZ".to_string(),
        };
        let _ = db.set_binary_tags(
            id,
            &[BinaryTag {
                key,
                payload: vec![0xCDu8; 64],
                ordinal: 0,
            }],
        );
    }
```

- [ ] **Step 5: Add the fuzzer-gated hostility selection + pre-resolve mutation**

Immediately after the binary-tag block (still before `let resolved = …`), add:

```rust
    // Hostile-row stage. Variants 0/1/3 corrupt geometry/format/art-metadata that
    // `resolve` validates (it returns Err -> the existing early-return below
    // handles them). Variants 2/4/5 are read-time hostilities applied AFTER a
    // successful resolve.
    let hostile = if u.arbitrary::<bool>().unwrap_or(false) {
        Some(u.int_in_range(0..=5u8).unwrap_or(0))
    } else {
        None
    };
    let hostile_val = u.arbitrary::<i64>().unwrap_or(i64::MAX);
    if matches!(hostile, Some(0 | 1 | 3)) {
        apply_hostile(&db, id, hostile.unwrap(), hostile_val);
    }
```

- [ ] **Step 6: Apply post-resolve hostility and soften the reads (assertion discipline)**

The current tail of the target is:

```rust
    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };
    let total = resolved.total_len;
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");

    // The single whole read every window is checked against (splice consistency).
    let whole = read_at_with_file(&resolved, &db, &file, 0, total).unwrap();
    assert_eq!(whole.len() as u64, total, "whole read length != total_len");
```

Replace it with (note: `resolve` already returns early on Err, which covers variants 0/1/3):

```rust
    let resolved = match HeaderCache::new(Mode::Synthesis).resolve(&db, id) {
        Ok(r) => r,
        Err(_) => return,
    };

    // Read-time hostilities: apply only after a successful resolve.
    let hostile_post = matches!(hostile, Some(2 | 4 | 5));
    if hostile_post {
        apply_hostile(&db, id, hostile.unwrap(), hostile_val);
    }

    let total = resolved.total_len;
    let file = std::fs::File::open(&resolved.backing_path).expect("backing file opens");

    // Splice-consistency invariants. A successfully-resolved layout is internally
    // consistent regardless of how its rows were planted, so whenever the read
    // returns Ok these MUST hold and are asserted. The only hostile-path
    // relaxation: a read may return Err (missing art row, stale binary-tag
    // handle, bumped content_version) -> return/break, do not assert.
    let whole = match read_at_with_file(&resolved, &db, &file, 0, total) {
        Ok(w) => w,
        Err(_) if hostile_post => return,
        Err(e) => panic!("clean-path whole read failed: {e:?}"),
    };
    assert_eq!(whole.len() as u64, total, "whole read length != total_len");
```

- [ ] **Step 7: Soften the per-window read inside the read loop**

In the `for _ in 0..8 { … }` loop, the window read is currently:

```rust
        let got = read_at_with_file(&resolved, &db, &file, offset, size).unwrap();
```

Replace it with:

```rust
        let got = match read_at_with_file(&resolved, &db, &file, offset, size) {
            Ok(g) => g,
            Err(_) if hostile_post => break,
            Err(e) => panic!("clean-path window read failed: {e:?}"),
        };
```

(The existing clamp assertions on `got` that follow stay unchanged — they still run whenever the read returns `Ok`.)

- [ ] **Step 8: Add the `apply_hostile` helper**

After the `setup` function (before the `fuzz_target!` macro), add:

```rust
/// Plant one hostile mutation via the fuzzing-only raw accessor. The production
/// read path must reject the resulting state with `Err`, never UB. Each fuzz
/// iteration uses a fresh in-memory DB (`setup`), so dropping a trigger / leaving
/// a pragma toggled is scoped to that iteration's connection.
///
/// Variants whose target row ALWAYS exists (0, 1, 5 — the single `tracks` row)
/// assert `n == 1` so a future schema rename cannot silently turn the mutation
/// into a swallowed no-op (this exact trap hid a `backing_mtime` -> `backing_mtime_ns`
/// rename during planning). Variants 2/3/4 are genuinely conditional (they need an
/// art row or binary-tag row that the earlier stages may not have created), so
/// they stay best-effort (`let _ =`).
fn apply_hostile(db: &Db, id: i64, variant: u8, val: i64) {
    db.with_raw_conn(|conn| match variant {
        // 0: negative/oversized integer geometry (resolve rejects at the bounds check).
        0 => {
            conn.execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            let n = conn
                .execute(
                    "UPDATE tracks SET audio_offset = ?1, audio_length = ?1 WHERE id = ?2",
                    rusqlite::params![val, id],
                )
                .unwrap();
            assert_eq!(n, 1, "variant 0 must mutate the tracks row");
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }
        // 1: invalid format discriminant (model deserialization must Err, not panic).
        1 => {
            conn.execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            let n = conn
                .execute(
                    "UPDATE tracks SET format = 'bogus' WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap();
            assert_eq!(n, 1, "variant 1 must mutate the tracks row");
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }
        // 2: orphaned track_art (art_id -> no art row) under FK off. No-op when no
        // art row was attached this iteration.
        2 => {
            conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            let _ = conn.execute(
                "UPDATE track_art SET art_id = 999999 WHERE track_id = ?1",
                rusqlite::params![id],
            );
            conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        }
        // 3: oversized art mime. `art` rows are immutable via the
        // `art_reject_content_update` trigger, and `ignore_check_constraints` does
        // NOT disable triggers, so the trigger must be dropped first (the length(mime)
        // CHECK still needs the pragma). No-op when no art row was attached.
        3 => {
            conn.execute_batch(
                "PRAGMA ignore_check_constraints = ON; DROP TRIGGER art_reject_content_update;",
            )
            .unwrap();
            let _ = conn.execute(
                "UPDATE art SET mime = ?1 \
                 WHERE id IN (SELECT art_id FROM track_art WHERE track_id = ?2)",
                rusqlite::params!["x".repeat(100_000), id],
            );
            conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }
        // 4: stale binary-tag handle (delete the blob rows the layout will read).
        // No-op unless the binary-tag opt-in fired for an MP3/M4a track.
        4 => {
            let _ = conn.execute(
                "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
                rusqlite::params![id],
            );
        }
        // 5: backing-geometry / content-version mismatch (per-read freshness guard).
        _ => {
            let n = conn
                .execute(
                    "UPDATE tracks SET backing_size = backing_size + 1, \
                     backing_mtime_ns = backing_mtime_ns + 1, \
                     content_version = content_version + 1 WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap();
            assert_eq!(n, 1, "variant 5 must mutate the tracks row");
        }
    });
}
```

- [ ] **Step 9: Build the fuzz target**

Run: `cargo +nightly fuzz build serve`
Expected: builds clean (no unused-import/type errors).

- [ ] **Step 10: Smoke-run the target**

Run: `cargo +nightly fuzz run serve -- -runs=200000`
Expected: completes with no crash. A crash here means either (a) a new Ogg fixture does not synthesize on the clean path — inspect the panic message `clean-path … read failed` and fix the fixture's comment/STREAMINFO bytes in Task 4; or (b) a genuine production defect on a hostile row — that is a real finding to file, not to silence.

- [ ] **Step 11: Commit**

```bash
git add fuzz/Cargo.toml fuzz/fuzz_targets/serve.rs
git commit -m "$(cat <<'EOF'
test(fuzz): hostile rows, binary tags, distinct Ogg fixtures in serve (#313)

Fuzzer-gated hostility stage plants negative/oversized geometry, invalid
formats, orphaned/oversized art, stale binary-tag handles, and content-version
mismatches via the fuzzing-only raw accessor; optional set_binary_tags exercises
Segment::BinaryTag streaming; selectors 4/5/6 now feed distinct Opus/Vorbis/
OggFLAC fixtures. Splice invariants stay asserted whenever reads succeed; the
only relaxation is tolerating Err on the hostile path (reads no longer unwrap).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Documentation

**Files:**
- Modify: `CONTRIBUTING.md` (the coverage-guided-fuzzing section and the e2e/test-tiers section)

- [ ] **Step 1: Find the fuzzing + e2e doc anchors**

Run: `grep -n 'fuzz\|--ignored\|e2e\|end-to-end' CONTRIBUTING.md`
Expected: locates the coverage-guided-fuzzing subsection (lists the fuzz targets) and the test-tiers description of the `--ignored` e2e.

- [ ] **Step 2: Note the new serve fuzz scope**

In the coverage-guided-fuzzing section, where the `serve` target is described (or in the target list), add one sentence:

```markdown
The `serve` target also exercises hostile DB rows (negative/oversized geometry,
invalid formats, orphaned/oversized art, stale binary-tag handles, content-version
mismatch) via the `musefs-db` `fuzzing`-gated `with_raw_conn`, plus binary-tag
streaming and distinct Opus/Vorbis/OggFLAC fixtures.
```

- [ ] **Step 3: Note the new CI e2e steps**

Where the test tiers describe the `--ignored` FUSE e2e, add:

```markdown
The CI `e2e` job also runs the binary-level `cargo test -p musefs -- --ignored`
and `cargo test -p musefs-latencyfs -- --ignored` suites so they cannot silently
rot (they require `/dev/fuse` + `fusermount3`).
```

- [ ] **Step 4: Verify docs lint**

Run: `git add CONTRIBUTING.md && git diff --cached --stat`
Expected: only `CONTRIBUTING.md` staged. (Docs-only commit skips the cargo gate; ruff/yamllint do not apply to `.md`.)

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
docs: note expanded serve fuzz scope and new CI e2e steps (#320, #313)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] **Workspace suite green (what the pre-commit hook enforces):**
  Run: `cargo test --workspace` — Expected: all pass.
- [ ] **Metrics-feature core tests (CI `check` job; not covered by the bare workspace run):**
  Run: `cargo test -p musefs-core --features metrics` — Expected: all pass. (These assert exact getattr/read counts; this branch does not touch those paths, but the gate is cheap insurance.)
- [ ] **Clippy across all targets:**
  Run: `cargo clippy --all-targets -- -D warnings` — Expected: clean.
- [ ] **Format:**
  Run: `cargo fmt --all --check` — Expected: clean.
- [ ] **db fuzzing accessor:**
  Run: `cargo test -p musefs-db --features fuzzing` — Expected: all pass.
- [ ] **Fuzz build + smoke (out-of-workspace):**
  Run: `cargo +nightly fuzz build serve && cargo +nightly fuzz run serve -- -runs=200000` — Expected: builds; no crash.
- [ ] **Ignored e2e on this `/dev/fuse` host:**
  Run: `cargo test -p musefs --test sigterm_unmount -- --ignored --test-threads=1`
  Run: `cargo test -p musefs-fuse --test read_consistency write_ops_are_refused_on_read_only_mount -- --ignored`
  Run: `cargo test -p musefs-latencyfs -- --ignored --test-threads=1`
  Expected: all pass.
