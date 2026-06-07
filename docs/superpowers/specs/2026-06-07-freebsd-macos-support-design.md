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

## Scope split: one spec, two plans

This design is a single coherent document, but it covers two largely
independent features that should become **two implementation plans**:

- **Plan A (compile-time platform axis):** the `platform` module, passthrough
  gating, per-OS mount options, the macOS Spotlight marker, and FreeBSD e2e/CI.
  Low-risk, mechanical, verifiable on the FreeBSD VM.
- **Plan B (runtime case-insensitivity axis):** the `case_insensitive` flag and
  the tree case-folding work. Higher-risk; it touches the disambiguation and
  incremental-rebuild equivalence machinery and deserves its own TDD-driven plan
  with the `assert_apply_matches_build` invariant front and center.

Plan A can land first and independently; Plan B has no dependency on Plan A
beyond sharing the CLI/config plumbing.

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
  platform module): passthrough behavior, the Spotlight marker, the per-OS
  mount-option set.
- **Runtime, volume-dependent**: case-insensitivity, carried as a single config
  flag (APFS can be either case-sensitive or case-insensitive, so a compile-time
  assumption would be wrong even on macOS).

The stubs are written so that off-target handlers compile to nothing: e.g.
`platform::spotlight::marker_lookup(...)` returns `None` on non-macOS, and
`platform::passthrough::try_open_backing(...)` is a no-op on non-Linux.

## Components

### 1. Passthrough gating (`platform::passthrough`)

**Important correction to a naive assumption:** the fuser 0.17 passthrough
symbols (`InitFlags::FUSE_PASSTHROUGH`, `KernelConfig::set_max_stack_depth`,
`reply.open_backing`, `reply.opened_passthrough`) **already compile on every
OS** — they are not cfg-gated out by fuser. `BackingId::new` simply returns a
runtime error on non-Linux. So the motivation for the platform module here is
**design clarity and correct runtime behavior**, not compilation necessity.

Two concrete refactors:

- Today `init` (`musefs-fuse/src/lib.rs`) requests `FUSE_PASSTHROUGH` and sets
  `max_stack_depth` **unconditionally**. The Linux capability probes
  `cap_eff_has_sys_admin` / `definitely_lacks_cap_sys_admin` (`capget`/`prctl`)
  are also Linux-specific. These move behind `platform::passthrough`.
- `open` attempts `open_backing` and, on failure, sets the sticky
  `passthrough_disabled` flag and falls through to `reply.opened(...)`. This
  becomes a call to `platform::passthrough::try_open_backing(...)`.

Behavior per OS:

- **Linux:** verbatim current behavior — request the passthrough capability and
  stack depth in `init`; attempt `open_backing` in `open`; on failure set the
  sticky flag, log, and serve via the daemon.
- **Non-Linux:** the `init` tuning omits the passthrough request;
  `try_open_backing` is a no-op signalling "not attempted," so `open` always
  takes the userspace serving path.

`StructureOnly` mode remains selectable on every platform. On non-Linux it logs
once (reusing the existing message style, "FUSE passthrough unavailable; serving
reads through the daemon") and serves identical bytes via synthesis. This is the
**warn + fallback** behavior — and it lands on exactly the path that
`open` (`lib.rs:331`) already takes today when `open_backing` fails on an older
Linux kernel, so there is no new serving path to reason about.

`musefs-core`'s `passthrough_fd` / `PassthroughFd` are plain file descriptors and
remain cross-platform; only the fuser-side registration is gated.

### 2. Case-insensitivity (`case_insensitive` config flag)

This is the larger, riskier feature (Plan B). It must fold case consistently
across *every* place the tree compares names, including the incremental-rebuild
path, or it breaks the fresh-build equivalence invariant.

- **Config & CLI:** add `case_insensitive: bool` to `MountConfig`
  (`musefs-core/src/facade.rs:35`, alongside `mode`), plumbed from a new
  `--case-insensitive` CLI flag (`musefs-cli`). Its default is OS-derived:
  `true` on macOS, `false` on Linux/FreeBSD; user-overridable on any OS (e.g. a
  case-sensitive APFS volume → `--case-insensitive=false`). The flag threads
  `MountConfig → build_full / rebuild_full / rebuild_incremental → VirtualTree`.

- **Stored on the tree.** Because folding must be consulted by `lookup`,
  `disambiguate`, `collision_gate`, `ensure_dir`, and the subtree reconstruction
  in `apply_changes`, the flag is **stored as a field on `VirtualTree`** (and on
  the `InodeAllocator` where it keys by rendered path), not passed ad hoc. All
  four tree entry points must construct with the same value: `build`,
  `build_full`, `rebuild_full`, and `rebuild_incremental`.

- **Directory collisions → merge.** `ensure_dir` currently reuses an existing
  directory via exact `get(name)`. Under folding it reuses on the **folded** key,
  so two intermediate path components differing only by case
  (e.g. `Foo/` and `foo/`) collapse into **one** directory; the first-seen
  casing wins for display and the tracks live together. This mirrors a native
  case-insensitive filesystem and is the minimal generalization of the existing
  exact-name reuse.

- **Leaf collisions → disambiguate.** `disambiguate` / `collision_gate` /
  `insert_file` compare on the folded key, so two *files* that collide under
  folding are disambiguated by the same suffixing machinery that already handles
  exact collisions. Displayed names retain their original case.

- **Rendered index + inode allocation must fold consistently.** The
  rendered-child index, `InodeAllocator` (keyed by rendered path), and the
  disambiguation collision key must all use the folded key in lockstep. If the
  collision key folds but the rendered/allocator key does not, the
  `assert_apply_matches_build` equivalence test (incremental rebuild ==
  fresh build) breaks. This invariant is the acceptance gate for Plan B.

- **Lookup index.** When the flag is on, the tree maintains a parallel
  **folded-name index** (`parent → folded_name → inode`) consulted by folding
  the query name in `facade.rs::lookup` (`facade.rs:850`) — O(1). It is built
  **only** when the flag is on, so Linux/FreeBSD pay zero extra memory or CPU.
  When off, matching is byte-for-byte identical to today. (The index uses the
  same `im` persistent-map types as the existing children maps so structural
  sharing across rebuilds is preserved.)

- **Folding definition.** Use Unicode-aware case folding consistent with the
  host's expectations; ASCII case-insensitive is the floor. The exact folding
  function is an implementation detail of Plan B but must be applied uniformly
  to collision key, rendered key, allocator key, and lookup key.

**Consequence (intended):** with the flag on, a library with case-colliding
names presents a different tree than on Linux — case-variant directories merge,
case-variant files get disambiguated. This is inherent to a case-insensitive
namespace and is the correct trade-off.

### 3. `.metadata_never_index` marker (`platform::spotlight`, macOS only)

Compile-time macOS-only; no CLI flag. The mount is read-only, so the marker is
always desirable on macOS and never conflicts with a writer — an escape hatch is
unnecessary.

- **Reserved inode:** a sentinel constant `u64::MAX`. `InodeAllocator` starts at
  2 and only ever increments with no upper bound (`tree.rs:22`), so `u64::MAX`
  is unreachable in practice and cannot collide with a real node. (A fixed
  "high" constant like 1_000_000 would *not* be safe — there is no allocator
  ceiling to sit above.)
- **Shape:** a zero-byte regular file at the mount root.
- **Six interception points in `lib.rs`**, each delegating to a
  `platform::spotlight` helper that returns `None`/no-op on non-macOS so the
  handlers stay `#[cfg]`-free:
  1. `lookup(root, ".metadata_never_index")` → marker attr.
  2. `readdir(root)` → append the entry.
  3. `getattr(SENTINEL)` → marker attr.
  4. `open(SENTINEL)` → a handle that serves empty.
  5. `read(SENTINEL, ...)` → 0 bytes (EOF).
  6. `release(SENTINEL)` → no-op (no core handle to release).
- **Marker attributes** (`to_file_attr` inputs): `FileType::RegularFile`, mode
  `0o444` (read-only), `nlink = 1`, `size = 0`, `blocks = 0`, owner = the
  mount's `uid`/`gid`, all timestamps = the mount time (matching how other
  synthetic nodes are stamped).

On Linux and FreeBSD every helper is a stub returning `None`, so the marker does
not exist and the handlers are unaffected.

### 4. Mount options (`platform::mount`)

`mount_config` (`musefs-fuse/src/lib.rs:485`) delegates to
`platform::mount::options(fs_name, case_insensitive)`:

- **Common:** `MountOption::RO`, `MountOption::FSName`.
- **macOS (best-effort, tunable):** fuser 0.17 has **no** `VolName` variant, so
  macOS-specific options go through `MountOption::CUSTOM(...)`: e.g.
  `CUSTOM("volname=<name>")`, AppleDouble suppression
  (`CUSTOM("noappledouble")`), and a case-sensitivity hint where the backend
  honors it. Marked tunable because FUSE-T's option set differs from macFUSE and
  is the least-verified surface.
- **FreeBSD:** the common set only (verified against the local VM).

## Data flow (unchanged core)

The segment model, `read_at`, synthesis, and the store contract are platform
agnostic and unchanged. Platform differences are confined to: which mount
options are requested, whether passthrough registration is attempted, how a
child name is matched on lookup, and the presence of one synthetic root entry on
macOS.

## Error handling

- Mount failures surface as today. On macOS built with fuser's `macos-no-mount`
  feature (see Testing), `Mount::new` is a compiled-out stub that returns a
  runtime error — acceptable for the best-effort, no-mount CI configuration.
- Requesting `StructureOnly` on a platform/kernel without passthrough is **not**
  an error: it warns once and falls back to userspace serving.
- Case-insensitive lookups that miss return `ENOENT` exactly as exact-match
  lookups do.

## Testing strategy

- **FreeBSD (real e2e):** a gitignored `.scratch/` directory holds a small
  FreeBSD VM image for local testing. CI uses a FreeBSD VM GitHub action that
  loads the **`fusefs` kernel module** and runs the `--ignored` FUSE e2e suite.
  (FreeBSD resolves to fuser's `pure-rust` mount backend, which talks to
  `/dev/fuse` directly and does **not** link libfuse — so installing
  `fusefs-libfuse` is unnecessary; confirm the exact module/package against the
  VM before finalizing the workflow.) The new job **must be added to the
  `ci-ok` aggregator's `needs:` array** (`.github/workflows/ci.yml:207`,
  currently `[changes, check, interop, python-musefs, beets, picard, e2e]`) —
  otherwise it runs but cannot block merges.
- **macOS (best-effort):** the CI runner does `cargo build` / `cargo clippy` /
  `cargo test` (unit + non-mount integration) only. A stock macOS build
  **fails in fuser's `build.rs`**, which `pkg-config`-probes for macFUSE unless
  fuser's **`macos-no-mount`** feature is enabled. The macOS build therefore
  enables `fuser/macos-no-mount`; under it `Mount::new` is a test-only stub and
  no mount is attempted. The case-folding/disambiguation logic and the marker
  helpers are covered by unit tests that toggle the runtime flag, so they are
  exercised on every OS even where we cannot mount.
- **fuzz crate:** unaffected. Changes live in `musefs-core`/`musefs-cli`/
  `musefs-fuse`, not the `musefs-format` API surface that the out-of-workspace
  fuzz targets bind to.
- All new platform code follows the workspace integer-cast convention
  (`usize_from` and friends).

## Open questions (resolve during implementation)

- **FUSE-T vs macFUSE for a *real* macOS build.** Does FUSE-T satisfy fuser's
  `pkg-config "fuse"` probe, or does a FUSE-T-backed (non-`macos-no-mount`)
  build need a different link/probe path? Out of scope for the best-effort CI
  build, but determines whether musefs can actually mount on macOS later.

## Out of scope

- Mounted macOS e2e validation (deferred; best-effort compile + unit coverage for
  now).
- Auto-detecting the host volume's case-sensitivity (explicitly rejected in
  favor of the OS-defaulted, user-overridable flag).
- Any change to the read-only invariant or the store contract.
