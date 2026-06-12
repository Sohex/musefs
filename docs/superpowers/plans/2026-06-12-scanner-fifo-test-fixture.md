# Scanner FIFO Test Fixture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Unix-socket fixture in the scanner symlink-to-non-file hardening test with a `libc::mkfifo` FIFO so the test stops failing in sandboxes that deny Unix-socket `bind` (issue #277).

**Architecture:** Test-fixture-only change. The product code (`collect_audio`) is untouched. A FIFO is, like a Unix socket, neither a regular file nor a directory, so it exercises the identical `is_file()` branch the test asserts on — but `mkfifo` works in restricted environments that block socket `bind`. The single FFI call is wrapped in the workspace's documented `#[expect(unsafe_code, …)]` pattern (the workspace lints `unsafe_code = "deny"`).

**Tech Stack:** Rust, `libc` 0.2 (already a workspace dep, added here as a `musefs-core` dev-dependency), `tempfile`.

---

### Task 1: Swap the Unix-socket fixture for a FIFO

**Files:**
- Modify: `musefs-core/Cargo.toml` (`[dev-dependencies]`)
- Modify: `musefs-core/src/scan.rs` — fn `collect_audio_ignores_symlink_to_non_file_target_when_following` (currently lines 1549-1565)

This is an existing test whose *assertion* already pins the behavior under test (a symlink to a non-file, non-dir target must not be collected). We are changing only the mechanism that creates the non-file target, so there is no new product behavior to drive with a fresh failing test. The verification is: the rewritten test compiles, still passes (behavior preserved), and no longer calls `UnixListener::bind`.

- [ ] **Step 1: Add `libc` as a dev-dependency**

In `musefs-core/Cargo.toml`, under the existing `[dev-dependencies]` block, add the line (alongside `tempfile`, `metaflac`, etc.):

```toml
libc = "0.2"
```

- [ ] **Step 2: Rewrite the test fixture to use a FIFO**

In `musefs-core/src/scan.rs`, replace the entire body of
`collect_audio_ignores_symlink_to_non_file_target_when_following` with:

```rust
#[test]
fn collect_audio_ignores_symlink_to_non_file_target_when_following() {
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    // A FIFO is neither a regular file nor a directory, and mkfifo works in
    // restricted sandboxes that deny Unix-socket bind (issue #277).
    let fifo = dir.path().join("fifo");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    #[expect(unsafe_code, reason = "libc::mkfifo FFI; no std equivalent")]
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

    // Name the link with a supported audio extension so the only thing
    // keeping it out of `out` is the resolved target's is_file() check.
    std::os::unix::fs::symlink(&fifo, dir.path().join("link.flac")).unwrap();

    let mut out = Vec::new();
    collect_audio(dir.path(), &mut out, true).unwrap();
    assert!(
        out.is_empty(),
        "a symlink to a non-file, non-dir target must not be collected"
    );
}
```

- [ ] **Step 3: Run the test, verify it passes**

Run: `cargo test -p musefs-core scan::hardening_tests::collect_audio_ignores_symlink_to_non_file_target_when_following -- --exact`
Expected: PASS (`test result: ok. 1 passed`).

- [ ] **Step 4: Run the full scanner test subset**

Run: `cargo test -p musefs-core scan::`
Expected: PASS — this is the exact subset the issue reported as failing.

- [ ] **Step 5: Confirm the socket bind is gone**

Run: `grep -n "UnixListener" musefs-core/src/scan.rs`
Expected: no output (no remaining `UnixListener` references in the file).

- [ ] **Step 6: Lint — the `#[expect(unsafe_code)]` must be fulfilled, not dangling**

Run: `cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: clean (no `unfulfilled_lint_expectations`, no warnings). Clippy compiles `--all-targets`, so the test module is linted here.

- [ ] **Step 7: Format**

Run: `cargo fmt`
Then `cargo fmt --all --check` — expected: clean exit (0).

- [ ] **Step 8: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
test(scan): use a FIFO, not a Unix socket, for the non-file target (#277)

UnixListener::bind needs a privilege some sandboxes deny, panicking the
test in setup. A FIFO is likewise neither a regular file nor a directory,
exercises the same is_file() branch, and mkfifo works where socket bind
does not. libc::mkfifo is wrapped in the workspace's sanctioned
#[expect(unsafe_code)] pattern; libc is added as a musefs-core dev-dep.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Notes for the implementer

- **Why `as_os_str().as_bytes()`:** `CString::new` needs bytes. On Unix, `OsStr::as_bytes` (from `std::os::unix::ffi::OsStrExt`, imported locally in the test) gives the path bytes directly without a lossy UTF-8 round-trip. The `tempfile` path has no interior NULs, so `CString::new` will not error.
- **Why mode `0o644`:** arbitrary; the FIFO is never opened/read. Any valid mode works. `mkfifo` returns `0` on success, `-1` on error (inspect `std::io::Error::last_os_error()` — handled by the assert message).
- **Do not** add a socket-then-FIFO fallback or any skip-on-error logic — out of scope per the spec; the FIFO alone is sufficient.
- **Do not** touch `collect_audio` or any other test; this is a single-fixture change.

## Self-review (done by plan author)

- **Spec coverage:** spec's three change points — `libc` dev-dep (Step 1), `mkfifo` fixture rewrite (Step 2), unchanged symlink + assertion (preserved verbatim in Step 2) — all present. Spec's verification items (`scan::` passes, clippy clean, behavior preserved) map to Steps 3-7.
- **Placeholders:** none; all code and commands are concrete.
- **Type consistency:** `libc::mkfifo(*const c_char, mode_t) -> c_int`; `c_path.as_ptr()` yields `*const c_char`; `0o644` coerces to `mode_t`; `rc` compared against `0`. `OsStrExt::as_bytes` returns `&[u8]` accepted by `CString::new`. Consistent throughout.
