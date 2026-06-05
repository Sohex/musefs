# CLI MountArgs grouping and Fh file-handle newtype

**Date:** 2026-06-05
**Issues:** #132 — CLI mount plumbing suppresses `too_many_arguments` instead
of grouping a config; #134 — FUSE file-handle non-zero invariant enforced by
arithmetic, not the type
**Status:** Approved

Two independent fixes covered by one spec, shipped as **two separate PRs**
with no ordering dependency (#132 is confined to `musefs-cli/src/lib.rs`;
#134 changes the `Musefs` facade surface in `musefs-core` and its consumers).

## Part 1 — #132: group the mount knobs into a clap `MountArgs` struct (PR 1)

### Problem

`parse_mount_config` (8 parameters) and `run_mount` (10) in
`musefs-cli/src/lib.rs` carry every mount knob positionally under
`#[allow(clippy::too_many_arguments)]`. The call chain re-lists the same
fields three times (the `Command::Mount` destructure in `run`, the
`run_mount` call, the `parse_mount_config` call), and the same-typed integer
knobs (`poll_interval_ms`, `attr_ttl_ms`, `max_readahead_kib`) are exposed to
silent argument-ordering mistakes at each site.

### Design

All changes in `musefs-cli/src/lib.rs`:

1. **New `#[derive(clap::Args, Debug)] pub struct MountArgs`** carrying all
   ten current `Mount` fields — `mountpoint`, `db`, `template`,
   `default_fallback`, `mode`, `poll_interval_ms`, `attr_ttl_ms`,
   `max_readahead_kib`, `max_background`, `keep_cache` — with their existing
   `#[arg(...)]` attributes and doc comments moved verbatim.
2. **`Command::Mount` becomes a tuple variant** `Mount(MountArgs)`.
3. **`parse_mount_config(args: &MountArgs) -> (MountConfig, FuseConfig)`** —
   the `#[allow]` is dropped. Stays pure and exported. The two `String`
   fields are cloned out of the borrow (once per mount; negligible), and the
   `mode: CliMode` converts via the existing `From<CliMode> for Mode` impl
   inside the function. Note the conversion *moves*: today `run` calls
   `mode.into()` before passing `Mode` down — that call site disappears
   along with the destructure.
4. **`run_mount(args: MountArgs) -> Result<()>`** — the `#[allow]` is
   dropped; `db` and `mountpoint` are read from the struct.
5. **`run`'s arm collapses** to `Command::Mount(args) => run_mount(args)`.

CLI behavior is byte-for-byte identical: `#[derive(clap::Args)]` on a
tuple-variant struct yields the same flags, defaults, and help text as the
inline variant fields.

### Testing

- The two existing scan tests' `Command::Mount { .. } => panic!(...)` arms
  become `Command::Mount(..)`.
- **One new unit test** (making `parse_mount_config`'s "exported for unit
  testing" doc comment finally true, and covering the in-diff mutation
  surface): `Cli::try_parse_from` a mount invocation with explicit
  `--poll-interval-ms`, `--attr-ttl-ms`, and `--max-readahead-kib` values →
  `parse_mount_config` → assert `MountConfig.poll_interval` and
  `FuseConfig.ttl` are the expected `Duration`s and `max_readahead` is the
  KiB value × 1024. This pins the ms→`Duration` and KiB→bytes conversions
  against mutants (`saturating_mul(1024)` → `*`/`/`/swap mutations).

## Part 2 — #134: `Fh` newtype owning the non-zero invariant (PR 2)

### Problem

`fh_from_key` in `musefs-core/src/facade.rs` offsets sharded-slab keys by
`+1` so the kernel never sees a file handle of 0 (`fh == 0` is musefs's "no
handle" convention — `read` falls back to inode resolution). The reverse
`-1` lives at two other sites (`read_into`, `release_handle`), each guarded
by an `fh != 0` sentinel check. The invariant exists only as arithmetic in
three places plus a comment.

### Design

**New public type in `musefs-core/src/facade.rs`:**

```rust
/// A FUSE file handle: the sharded-slab key offset by one, so the wire
/// value is never 0 (0 on the wire means "no handle").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fh(NonZeroU64);

impl Fh {
    fn from_slab_key(key: usize) -> Fh   // the +1 — sole site
    fn slab_key(self) -> usize           // the -1 — sole site
    pub fn get(self) -> u64              // wire value for the FUSE layer
}
impl From<NonZeroU64> for Fh             // wire → type at the FUSE boundary
```

`from_slab_key` constructs via `NonZeroU64::MIN.saturating_add(key as u64)`
— panic-free, overflow-proof, and non-zero by construction. The two
conversion methods are private: the offset arithmetic cannot leak outside
the type.

**Facade API changes:**

- `open_handle(...) -> Result<Fh>` — the `fh_from_key` helper stays (its
  `None` arm is only unit-testable as a standalone function; a full slab
  can't practically be produced in a test) but becomes
  `fn fh_from_key(key: Option<usize>) -> Result<Fh>`, i.e.
  `key.map(Fh::from_slab_key).ok_or(CoreError::HandleTableFull)` — no
  arithmetic of its own anymore.
- `read` / `read_into` take `fh: Option<Fh>` — the `fh != 0` sentinel check
  becomes `if let Some(fh)`, slab lookup via `fh.slab_key()`. `None`
  preserves today's fallback to inode resolution.
- `release_handle(fh: Fh)` — the sentinel guard disappears; an absent handle
  is unrepresentable at the call site.

Stale-handle semantics are unchanged: a `Some(fh)` whose generation-encoded
slab slot was reused or removed still misses the slab lookup and falls back
exactly as a raw stale `u64` does today.

**FUSE layer (`musefs-fuse/src/lib.rs`)** converts at the wire boundary:

- `open`: `reply.opened(FileHandle(fh.get()), flags)`.
- `read`: passes `NonZeroU64::new(fh.0).map(Fh::from)` into `read_into`.
- `release`: converts the same way and skips the core call on `None`,
  preserving today's `fh == 0` no-op.

**Ripple — test updates in `musefs-core`:** `tests/facade.rs`,
`tests/flac_binary_tags.rs`, `tests/metrics.rs`, `tests/bench_ingest.rs`,
`benches/read_throughput.rs` (compiled only under `--all-targets` — easy to
miss), and the two handle-reuse tests in `src/facade.rs`'s own `#[cfg(test)]`
module switch `fs.read(inode, 0, …)` → `fs.read(inode, None, …)` and
`fs.read(inode, fh, …)` → `fs.read(inode, Some(fh), …)`. Two assertions are
not mechanical and are **deleted**: the `assert!(fh != 0)` at
`tests/facade.rs:347` and `assert!(fh1 != 0 && fh2 != 0)` at `:965` no
longer type-check against `Fh` — the `NonZeroU64` wrapper subsumes them.
The handle-distinctness checks (`assert_ne!` at `:374` and `:964`) are
retained; they rely on the `PartialEq, Eq` derives on `Fh`, which are
load-bearing for exactly this reason.

### Testing

The existing `fh_from_key_offsets_by_one_and_maps_full_to_error` unit test
is rewritten against the newtype:

1. **Round-trip:** `Fh::from_slab_key(k).slab_key() == k` for `k = 0` and a
   non-trivial key, and `Fh::from_slab_key(0).get() == 1` /
   `from_slab_key(41).get() == 42` (pins the wire offset).
2. **Capacity error:** `fh_from_key(None)` still maps to
   `CoreError::HandleTableFull`, as today.

The non-zero half of the old test ("fh is always non-zero") needs no
runtime assertion anymore — `NonZeroU64` makes it unrepresentable.

## Validation (both PRs)

`cargo test` (workspace), `cargo clippy --all-targets`,
`cargo fmt --all --check`, and the in-diff mutation gate
(`cargo mutants --in-diff … -j2`) per CLAUDE.md. The FUSE e2e tests
(`cargo test -p musefs-fuse -- --ignored`) are worth one run on PR 2 since
it touches the open/read/release path.
