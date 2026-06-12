# Scanner hardening test: FIFO fixture instead of Unix socket

**Issue:** [#277](https://github.com/Sohex/musefs/issues/277)
**Date:** 2026-06-12

## Problem

The scanner unit test
`scan::hardening_tests::collect_audio_ignores_symlink_to_non_file_target_when_following`
builds its fixture with `std::os::unix::net::UnixListener::bind`. Some restricted
test environments (audit sandboxes, certain CI) deny Unix socket creation, so the
bind fails with `PermissionDenied` and the test panics in setup:

```text
called `Result::unwrap()` on an `Err` value: Os { code: 1, kind: PermissionDenied, message: "Operation not permitted" }
```

This is an environmental failure unrelated to scanner behavior.

## What the test actually asserts

`collect_audio` follows symlinks (when `follow_symlinks` is true) and keeps a
candidate only if the **resolved target is a regular file**. The fixture needs a
target that is *neither a regular file nor a directory* so that the target's
`is_file()` check is the sole reason `link.flac` is excluded. The Unix socket is
incidental — any non-file, non-directory inode satisfies the assertion.

A **FIFO** (named pipe) is also neither a regular file nor a directory, exercises
the identical product-code branch, and is created with `mkfifo`, a basic
filesystem syscall that restricted environments rarely block (the sandboxes that
deny socket `bind` are restricting network primitives, not `mkfifo`).

## Decision

Replace the Unix socket with a FIFO outright (not a fallback). The socket and FIFO
are interchangeable for what this test verifies, and the FIFO works in strictly
more environments, so a socket-primary/FIFO-fallback path would add two code paths
for no coverage benefit.

`mkfifo` is not in `std`. Of the options (`libc::mkfifo`, shell out to `mkfifo(1)`,
`nix::unistd::mkfifo`), use **`libc::mkfifo`**:

- `libc` is already a workspace dependency (direct in `musefs-fuse` and
  `musefs-latencyfs`, locked at `0.2`); adding it as a `dev-dependency` of
  `musefs-core` is consistent and cheap.
- It is the most direct route and the most robust against restricted environments
  (no subprocess, no external coreutils binary).
- The single FFI call is wrapped in the repo's documented, greppable
  `#[expect(unsafe_code, reason = "…")]` pattern, satisfying the workspace-wide
  `unsafe_code = "deny"` lint.

## Changes

### `musefs-core/Cargo.toml`

Add under `[dev-dependencies]`:

```toml
libc = "0.2"
```

### `musefs-core/src/scan.rs`

In `collect_audio_ignores_symlink_to_non_file_target_when_following`, replace the
two `UnixListener` lines:

```rust
let sock = dir.path().join("sock");
let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
```

with a FIFO created via `libc::mkfifo`:

```rust
// A FIFO is neither a regular file nor a directory, and mkfifo works in
// restricted sandboxes that deny Unix-socket bind (issue #277).
let fifo = dir.path().join("fifo");
let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
#[expect(unsafe_code, reason = "libc::mkfifo FFI; no std equivalent")]
let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
```

The symlink (`link.flac` → the FIFO), the `collect_audio(..., true)` call, and the
`out.is_empty()` assertion are unchanged. `as_os_str().as_bytes()` requires
`std::os::unix::ffi::OsStrExt`, brought in with a local `use`.

## Out of scope

- No fallback machinery (socket-then-FIFO). The FIFO alone is sufficient.
- No changes to `collect_audio` or any product code; this is a test-fixture fix.
- No skip-on-error logic; the test still asserts on every supported host.

## Verification

- `cargo test -p musefs-core scan::` passes.
- `cargo clippy --all-targets` is clean (the `#[expect(unsafe_code, …)]` is
  satisfied, not unfulfilled).
- The assertion still fails if `collect_audio` were to wrongly include a
  non-file symlink target (the behavior under test is preserved).
