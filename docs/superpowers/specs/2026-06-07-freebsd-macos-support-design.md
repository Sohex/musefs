# FreeBSD and macOS (FUSE-T) Support — Design

**Date:** 2026-06-07
**Status:** Approved (pending implementation plan)

## Goal

Make musefs build and run on FreeBSD and macOS (via FUSE-T) in addition to
Linux. FreeBSD is the near-trivial lift (`fuser` already supports it, no case or
Spotlight concerns) and gets real end-to-end coverage. macOS is best-effort for
now: it must compile and its platform-specific logic must be exercised by unit
tests, but mounted e2e validation is deferred.

Two platform realities drive the design:

1. **Passthrough is Linux-only.** `StructureOnly` mode relies on kernel FUSE
   passthrough (`FUSE_PASSTHROUGH`), which exists only on Linux 6.9+.
2. **macOS volumes are commonly case-insensitive**, while musefs currently
   assumes case-sensitive exact-name matching. macOS also wants a
   `.metadata_never_index` marker at the root so Spotlight does not try to index
   the mount.

The cardinal musefs invariant is untouched: original audio bytes are never
copied or modified on any platform.

## Architecture: centralized platform module (Approach B)

A new `musefs-fuse/src/platform/` module is the single home for all per-OS
behavior. `lib.rs` stops branching on the operating system and calls into this
module through an OS-agnostic interface; its FUSE handlers contain **no**
`#[cfg]` attributes.

```
musefs-fuse/src/platform/
  mod.rs          // re-exports; the OS-agnostic interface lib.rs calls
  passthrough.rs  // Linux: real impl; other OSes: no-op stubs
  spotlight.rs    // macOS: .metadata_never_index marker; other OSes: stubs
  mount.rs        // per-OS MountOption list builder
```

The split between compile-time and runtime axes:

- **Compile-time per-OS** (handled by `#[cfg(target_os = ...)]` *inside* the
  platform module): passthrough availability, the Spotlight marker, the
  per-OS mount-option set.
- **Runtime, volume-dependent**: case-insensitivity, carried as a single config
  flag (APFS can be either case-sensitive or case-insensitive, so a compile-time
  assumption would be wrong even on macOS).

The stubs are written so that off-target handlers compile to nothing: e.g.
`platform::spotlight::marker_lookup(...)` returns `None` on non-macOS, and
`platform::passthrough::try_open_backing(...)` is a no-op on non-Linux.

## Components

### 1. Passthrough gating (`platform::passthrough`)

Today `init`/`open` (`musefs-fuse/src/lib.rs`) directly reference Linux-only
fuser symbols and Linux capability probes:

- `InitFlags::FUSE_PASSTHROUGH`, `KernelConfig::set_max_stack_depth`
- `reply.open_backing(...)`, `reply.opened_passthrough(...)`
- `cap_eff_has_sys_admin`, `definitely_lacks_cap_sys_admin` (which use
  `capget`/`prctl`)

All of these move into `platform::passthrough`:

- **Linux:** behavior verbatim — request the passthrough capability and stack
  depth in `init`; attempt `open_backing` in `open`; on failure set the sticky
  `passthrough_disabled` flag and log, then serve via the daemon.
- **Non-Linux:** the `init` tuning omits the passthrough bits; `try_open_backing`
  is a no-op signalling "not attempted," so `open` always takes the userspace
  serving path.

`StructureOnly` mode remains selectable on every platform. On non-Linux it logs
once (reusing the existing message style, "FUSE passthrough unavailable; serving
reads through the daemon") and serves identical bytes via synthesis. This is the
**warn + fallback** behavior — and it is the *same* fallback that older Linux
kernels (no passthrough support) already exercise, so there is no new serving
path to reason about.

`musefs-core`'s `passthrough_fd` / `PassthroughFd` are plain file descriptors and
remain cross-platform; only the fuser-side registration is gated.

### 2. Case-insensitivity (`case_insensitive` config flag)

- **Config & CLI:** add `case_insensitive: bool` to the core config, plumbed from
  a new `--case-insensitive` CLI flag (`musefs-cli`). Its default is OS-derived:
  `true` on macOS, `false` on Linux/FreeBSD. The user can override it on any OS
  (e.g. a case-sensitive APFS volume → `--case-insensitive=false`).
- **Tree build (`musefs-core/src/tree.rs`):** the existing sibling-collision
  disambiguation generalizes to compare on a **case-folded** key when the flag is
  set. Two siblings that differ only by case (e.g. an album tagged `Foo` and
  another `foo`) are disambiguated by the same machinery that already handles
  exact-name collisions. Displayed names retain their original case.
- **Lookup (`musefs-core/src/facade.rs::lookup`):** when the flag is on, the tree
  maintains a parallel **folded-name index** (`parent → folded_name → inode`)
  consulted by folding the query name — O(1). This index is built **only** when
  the flag is on, so Linux/FreeBSD pay zero extra memory or CPU. When off,
  matching is byte-for-byte identical to today.

**Consequence (intended):** with the flag on, a library containing
case-colliding siblings presents slightly different (disambiguated) names than on
Linux. This is inherent to a case-insensitive namespace and is the correct
trade-off.

### 3. `.metadata_never_index` marker (`platform::spotlight`, macOS only)

Compile-time macOS-only; no CLI flag. The mount is read-only, so the marker is
always desirable on macOS and never conflicts with a writer — an escape hatch is
unnecessary.

- A reserved high inode constant (chosen above the allocator's range so it never
  collides with a real node).
- The marker is a **zero-byte regular file at the mount root**.
- FUSE handlers consult `platform::spotlight` helpers that return `None` on
  non-macOS:
  - `readdir(root)` appends the entry.
  - `lookup(root, ".metadata_never_index")` and `getattr(marker_ino)` return its
    attributes.
  - `open` / `read` serve an empty file (read → 0 bytes / EOF); `release`
    no-ops.

On Linux and FreeBSD every helper is a stub returning `None`, so the marker does
not exist and the handlers are unaffected.

### 4. Mount options (`platform::mount`)

`mount_config` (`musefs-fuse/src/lib.rs`) delegates to
`platform::mount::options(fs_name, case_insensitive)`:

- **Common:** `MountOption::RO`, `MountOption::FSName`.
- **macOS (best-effort, tunable):** add `VolName`; suppress AppleDouble noise
  (`noappledouble`-style options via `MountOption::CUSTOM`); pass a
  case-sensitivity hint where the backend honors it. Marked tunable because
  FUSE-T's option set differs from macFUSE and is the least-verified surface.
- **FreeBSD:** the common set only (verified against the local VM).

## Data flow (unchanged core)

The segment model, `read_at`, synthesis, and the store contract are platform
agnostic and unchanged. Platform differences are confined to: which mount
options are requested, whether passthrough registration is attempted, how a
child name is matched on lookup, and the presence of one synthetic root entry on
macOS.

## Error handling

- Mount failures surface as today (the racy fusermount handshake serialization in
  `new_session` is Linux/FreeBSD relevant; macOS/FUSE-T uses its own mechanism
  but the same `Session::new` entry point).
- Requesting `StructureOnly` on a platform/kernel without passthrough is **not**
  an error: it warns once and falls back to userspace serving.
- Case-insensitive lookups that miss return `ENOENT` exactly as exact-match
  lookups do.

## Testing strategy

- **FreeBSD (real e2e):** a gitignored `.scratch/` directory holds a small
  FreeBSD VM image for local testing. CI uses a FreeBSD VM GitHub action that
  installs `fusefs-libfuse`, enables `fusefs`, and runs the `--ignored` FUSE e2e
  suite. The job slots into the existing `ci-ok` aggregator required by the
  branch-protection ruleset.
- **macOS (best-effort):** the CI runner does `cargo build` / `cargo clippy` /
  `cargo test` (unit + non-mount integration) only. No mount step — macFUSE needs
  a kext approval and FUSE-T a signed pkg, neither CI-friendly. The
  case-folding/disambiguation logic and the marker helpers are covered by unit
  tests that toggle the runtime flag, so they are exercised on every OS even
  where we cannot mount.
- **fuzz crate:** unaffected. Changes live in `musefs-core`/`musefs-cli`/
  `musefs-fuse`, not the `musefs-format` API surface that the out-of-workspace
  fuzz targets bind to.
- All new platform code follows the workspace integer-cast convention
  (`usize_from` and friends).

## Out of scope

- Mounted macOS e2e validation (deferred; best-effort compile + unit coverage for
  now).
- Auto-detecting the host volume's case-sensitivity (explicitly rejected in
  favor of the OS-defaulted, user-overridable flag).
- Any change to the read-only invariant or the store contract.
