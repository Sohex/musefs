# Eliminate `unsafe` code and enforce it workspace-wide

**Date:** 2026-06-08
**Status:** Approved (design)

## Goal

Remove every `unsafe` block from the workspace `src` and test code, replacing
the underlying libc FFI with safe `rustix` calls, and add a workspace-level lint
so that `unsafe` is denied by default everywhere. `unsafe` remains *permitted*
when genuinely necessary, but only via a visible, greppable per-site opt-in — it
can never be introduced silently.

## Background

A sweep of the workspace (`fuzz/` is excluded and out of scope — it is not a
workspace member) finds exactly five `unsafe` sites, all thin libc FFI:

| # | Location | Current code |
|---|----------|--------------|
| 1 | `musefs-fuse/src/lib.rs:189-190` | `unsafe { libc::getuid() }` / `getgid()` |
| 2 | `musefs-latencyfs/src/lib.rs:233-234` | `unsafe { libc::getuid() }` / `getgid()` |
| 3 | `musefs-latencyfs/src/lib.rs:425-429` | `MaybeUninit::<libc::statvfs>` + `unsafe libc::statvfs` + `assume_init` |
| 4 | `musefs-latencyfs/tests/passthrough.rs:109-111` | `mem::zeroed()` + `unsafe libc::statvfs` |
| 5 | `musefs-fuse/tests/concurrency.rs:127` | `unsafe { std::env::set_var("MUSEFS_FAULT_PREAD_US", "50000") }` |

Notes:

- `rustix 1.1.4` is **already** in the dependency tree (pulled by `tempfile`),
  so making it a direct dependency adds essentially zero compile cost.
- `libc` stays a dependency: it is still used for *safe* errno constants
  (`libc::EIO`, `libc::ENOENT`, …) in both crates. Only the `unsafe` FFI calls
  are removed, not the constant references.
- Site 5 compiles today only because the whole test file is
  `#![cfg(feature = "metrics")]` and the lint pass does not enable that feature,
  so the block is currently invisible. The `unsafe` was written defensively for
  edition 2024, where `std::env::set_var` is genuinely `unsafe`.

## Approach

### Crate choice: `rustix`

`rustix` covers all three distinct operations (`getuid`, `getgid`, `statvfs`)
with one already-compiled, soundness-audited crate. On Linux it uses raw
syscalls (no libc indirection); on FreeBSD/macOS it uses the libc backend
(matching the project's existing cross-platform support). Its `StatVfs` struct
normalises every field to `u64`, which additionally lets us delete the
per-platform cast workaround at site 3.

Alternatives considered and rejected:

- **nix** (also already in the tree): safe wrappers, but its `Statvfs` keeps
  platform-varying field types, so the cast cruft at site 3 would remain.
- **uzers/users**: purpose-built for uid/gid only — cannot do `statvfs`, would
  require a second new dependency or leave site 3 unsafe.

### Changes

**Dependencies**

- `musefs-fuse/Cargo.toml`: add `rustix = { version = "1", features = ["process"] }`
- `musefs-latencyfs/Cargo.toml`: add `rustix = { version = "1", features = ["process", "fs"] }`

**Site 1 & 2 — getuid/getgid**

Replace the `unsafe` block with:

```rust
uid: rustix::process::getuid().as_raw(),
gid: rustix::process::getgid().as_raw(),
```

`as_raw()` returns `RawUid`/`RawGid` (= `c_uint` = `u32`), which drops directly
into the existing `uid: u32` / `gid: u32` fields. The `// SAFETY:` comments are
removed.

**Site 3 — latencyfs `statfs`**

Replace the `MaybeUninit` / `assume_init` dance with:

```rust
if let Ok(s) = rustix::fs::statvfs(p) {
    return reply.statfs(
        s.f_blocks,
        s.f_bfree,
        s.f_bavail,
        s.f_files,
        s.f_ffree,
        u32::try_from(s.f_bsize).unwrap_or(u32::MAX),
        u32::try_from(s.f_namemax).unwrap_or(u32::MAX),
        u32::try_from(s.f_frsize).unwrap_or(u32::MAX),
    );
}
```

`rustix::fs::statvfs` takes a path directly (no `CString` construction needed),
so the manual `CString::new(OsStrExt::as_bytes(...))` step is also removed.
Because every `StatVfs` field is `u64`, the `#[allow(clippy::unnecessary_cast,
clippy::cast_lossless)]` attribute and its explanatory comment are deleted; the
`u32::try_from(...).unwrap_or(u32::MAX)` narrowings for the three `u32` reply
fields stay (they are genuine narrowings per the project's cast convention).

**Site 4 — latencyfs passthrough test**

Replace the `mem::zeroed` + `unsafe statvfs` with:

```rust
let s = rustix::fs::statvfs(mp).unwrap();
assert!(s.f_blocks > 0, "statfs should report real block counts");
```

The `CString` construction is no longer needed.

**Site 5 — fuse concurrency test + metrics setter**

The `MUSEFS_FAULT_OPEN_US` / `MUSEFS_FAULT_STAT_US` / `MUSEFS_FAULT_PREAD_US`
environment variables are a **documented benchmark interface** (see
`BENCHMARKS.md` and the SP0 measurement spec) and must keep working. The
benchmark harness sets them via the shell, so no Rust `set_var` is involved
there — only the one test mutates the environment from Rust.

In `musefs-core/src/metrics.rs` (inside the existing `#[cfg(feature =
"metrics")]` module):

1. Lift the three currently-function-local
   `static C: OnceLock<Option<Duration>>` cells (in `on_open`, `on_stat`,
   `on_pread`) to module scope (e.g. `OPEN_FAULT`, `STAT_FAULT`, `PREAD_FAULT`).
   `fault()` already takes the cell by reference, so its body is unchanged.
2. Add a public test/bench hook that pre-seeds a cell before its first read:

   ```rust
   /// Pre-seed the per-pread fault duration in-process, bypassing the
   /// `MUSEFS_FAULT_PREAD_US` env var. No-op if the cell was already read.
   pub fn set_fault_pread(d: Option<Duration>) {
       let _ = PREAD_FAULT.set(d);
   }
   ```

   (Only `set_fault_pread` is required by the current test; the open/stat
   setters are not added until a caller needs them — YAGNI.)

The `fault()` helper still falls back to `std::env::var` via `get_or_init` when
the cell was not pre-seeded, so the benchmark env-var path is unchanged.

The concurrency test replaces line 127 with:

```rust
musefs_core::metrics::set_fault_pread(Some(Duration::from_micros(50_000)));
```

This is deterministic (no env race), needs no environment mutation, and removes
the only Rust-side `set_var`.

### Enforcement

Add to the root `Cargo.toml`:

```toml
[workspace.lints.rust]
unsafe_code = "deny"
```

Every crate already inherits via `[lints] workspace = true`, so this applies
workspace-wide at once. `deny` (not `forbid`) is chosen deliberately: a future,
genuinely-necessary `unsafe` can be opted in per-site with a visible
`#[expect(unsafe_code, reason = "...")]`, which is greppable, shows up in review,
and (because it is `#[expect]`, not `#[allow]`) errors if the `unsafe` is later
removed but the annotation left behind. `forbid` would block even a justified
one-off until the whole-workspace lint were relaxed.

`fuzz/` is outside the workspace and is unaffected.

## Out of scope

- Migrating the workspace to edition 2024. (Noted only because edition 2024 is
  what makes `set_var` `unsafe`; this change happens to make that future
  migration cleaner, but the migration itself is a separate task.)
- Replacing `libc` errno *constants* with `rustix::io::Errno` — unnecessary
  churn; the constants are safe.

## Testing

- `cargo build` and `cargo test` (full workspace) stay green.
- `cargo clippy --all-targets` passes with the new `unsafe_code = "deny"` lint —
  this is the proof that no `unsafe` remains in any compiled target.
- `cargo clippy --all-targets --features metrics -p musefs-fuse` (the feature
  that gates the concurrency test) passes — confirms site 5's replacement
  compiles under the lint, which it does not today.
- The ignored real-mount concurrency test
  (`cargo test -p musefs-fuse --features metrics -- --ignored ...`) still
  demonstrates that a slow read does not block an unrelated stat, now driven by
  `set_fault_pread` instead of an env var.
- Cross-lint for FreeBSD via `--target x86_64-unknown-freebsd` per the project's
  existing CI parity check (rustix's `process`/`fs` modules are available there).

## Documentation

No user-facing docs change (the env-var benchmark interface is unchanged). If a
contributor-facing note on the `unsafe_code = "deny"` policy and the
`#[expect(unsafe_code, reason = "...")]` opt-in convention fits CONTRIBUTING.md's
conventions section, add it there.
