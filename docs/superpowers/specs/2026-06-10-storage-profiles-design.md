# Storage-aware mount profiles (`--storage-profile`) — design

*Date: 2026-06-10*

> **Outcome: NOT shipped.** Bench validation on real HDD and (loopback + `netem`) NFS
> disproved this spec's core premise — larger `max_readahead`/`max_background` give no
> benefit (and large read-ahead *hurts* on HDD); only `--keep-cache` helps. The preset
> was dropped; the four flags keep their defaults. See
> [BENCHMARKS.md §Storage tunables](../../../BENCHMARKS.md#storage-tunables). This
> document is retained for historical context only.

## Goal

musefs exposes four backing-media-sensitive mount tunables — `--max-readahead-kib`,
`--max-background`, `--attr-ttl-ms`, `--keep-cache` — but ships a single set of
hardcoded defaults and no guidance on what to set them to for a given backing store.
A user on a spinning disk or an NFS export is left to guess.

Add a `--storage-profile {ssd,hdd,nfs}` preset that sets all four tunables to
recommended values for that medium, and document in the README what each profile
sets and why. The values are derived from the in-tree latency model and **confirmed
empirically** with a kernel-mount latency bench before they ship.

## Non-goals

- No new tunables. This wires up presets over the four flags that already exist.
- No auto-detection of the backing medium. The user names their storage.
- NFS is a single conservative profile. We do **not** split `nfs-ssd` / `nfs-hdd`
  at the CLI: the per-RPC network tax dominates and the operator usually cannot
  tell (or control) the server's backing disk, so the profile targets the safe
  case. The bench still measures against both `nfs-ssd` and `nfs-hdd` latency
  settings to confirm the single profile is sane across that range.

## The four tunables (current state)

From `MountArgs` (`musefs-cli/src/lib.rs`), flowing into `FuseConfig`
(`musefs-fuse/src/lib.rs`) and applied at FUSE init via `set_max_readahead` /
`set_max_background` and the per-op cache TTLs:

| flag | current default | effect |
|---|---|---|
| `--max-readahead-kib` | 512 | kernel read-ahead window; bigger reads amortize seeks/RPCs over more bytes (clamped to the kernel max at mount) |
| `--max-background` | 64 | max outstanding background (read-ahead/async) requests the kernel keeps in flight |
| `--attr-ttl-ms` | 1000 | entry/attr cache TTL the kernel trusts before re-validating; bounds how fast external DB edits become visible |
| `--keep-cache` | off | keep the kernel page cache across opens (external re-tags auto-invalidate affected inodes) |

All four are **kernel↔FUSE negotiation / kernel-cache parameters**. They have no
effect on the in-process `read_at` path — see Validation.

## Latency model (the empirical basis)

`musefs-latencyfs` (`musefs-latencyfs/src/lib.rs::Latency::profile`) already encodes
the team's per-medium per-syscall latency model. The read-relevant rows (musefs is
read-only; `write`/`fsync` do not affect serving):

| op | ssd | hdd | nfs-ssd | nfs-hdd |
|---|---|---|---|---|
| read | ~0 | 8 ms | 600 µs | 8.6 ms |
| open | ~0 | 8 ms | 600 µs | 8.6 ms |
| stat | ~0 | 8 ms | 400 µs | 8.4 ms |

The gradient: SSD is seek-free; HDD pays an ~8 ms seek per cold read; NFS adds a
per-RPC network tax (sub-millisecond on an SSD backend, stacked on the HDD seek for
nfs-hdd). This maps directly onto the tunables — amortize seeks/RPCs with larger
read-ahead, hide latency with more in-flight requests, and cut metadata RPCs with a
longer attr TTL.

## Proposed values (bench hypothesis)

These are the starting point. The Validation run confirms or adjusts each number
before it is committed as the profile default; the final committed table is whatever
the bench supports.

| knob | ssd | hdd | nfs | rationale |
|---|---|---|---|---|
| `max_readahead_kib` | 512 | 2048 | 2048 | bigger reads amortize the 8 ms HDD seek / NFS RPC over more bytes |
| `max_background` | 64 | 64 | 128 | NFS hides network latency with more in-flight RPCs; HDD is seek-bound, gains nothing from more |
| `attr_ttl_ms` | 1000 | 2000 | 5000 | metadata ops are RPCs on NFS — a longer TTL is the biggest NFS win; tradeoff is slower visibility of external DB edits |
| `keep_cache` | off | on | on | avoid re-seeking a slow disk / re-fetching over the network on repeat opens |

`ssd` is the baseline and is intentionally ≈ today's hardcoded defaults.

## Flag semantics and precedence

Additive and backwards-compatible:

1. **No `--storage-profile`** → today's hardcoded defaults, unchanged. Existing
   command lines and their behavior are untouched.
2. **`--storage-profile X`** → the four tunables take profile X's values.
3. **An explicitly-passed knob overrides the profile** for that one knob, e.g.
   `--storage-profile nfs --max-readahead-kib 4096` keeps every nfs value except
   read-ahead.

### The clap subtlety (the one real code change beyond a lookup table)

`clap`'s `default_value_t` makes "user passed `--max-readahead-kib 512`"
indistinguishable from "defaulted to 512", so precedence rule 3 cannot be
implemented while those four fields carry literal defaults.

**Resolution:** change the four media-sensitive fields on `MountArgs` to `Option<…>`
(no `default_value_t`), meaning "unset on the command line". After parsing, a
`resolve_tunables(profile, args)` step folds them down:

```
effective = match (explicit_flag, profile) {
    (Some(v), _)        => v,                  // explicit always wins
    (None, Some(prof))  => prof.value(),       // else profile
    (None, None)        => HARDCODED_DEFAULT,  // else today's default
}
```

The hardcoded defaults move from `#[arg(default_value_t = …)]` into named constants
(`DEFAULT_MAX_READAHEAD_KIB`, etc.) reused by both `resolve_tunables` and the `ssd`
profile so the "ssd ≈ defaults" property is enforced in one place. `--help` text for
each flag gains a line noting it falls back to the profile / built-in default when
unset (so the disappearance of the literal default value stays self-documenting).

**`--keep-cache` is the exception** and needs care. It is currently a `store_true`
*presence* flag (`#[arg(long)] pub keep_cache: bool`) — presence means on, absence
means off, with no way to express "explicitly off". That two-state encoding cannot
support precedence rule 3: under `--storage-profile hdd` (which sets keep_cache on) a
user must be able to force it back *off*, which a presence flag cannot say. So
`keep_cache` becomes `Option<bool>` with `#[arg(long, action = clap::ArgAction::Set)]`
(no default) — the same value-taking form `case_insensitive` already uses
(`lib.rs`). This is a **UX change**: `--keep-cache` (bare) stops working; it becomes
`--keep-cache true|false`. Absent → `None` → profile/default. The README and the
flag's doc comment must call this out, and any docs/examples using bare
`--keep-cache` are updated.

*(Alternative considered and rejected: keep the existing flag types and detect
explicit settings via `ArgMatches::value_source`. It threads `ArgMatches` into
`parse_mount_config`, which the `cli.rs` tests bypass by constructing `MountArgs`
directly — so it is less testable — and, decisively, `value_source` still cannot
express "override keep_cache back to off" for a `store_true` flag. The `Option<…>`
approach keeps resolution a pure function of the parsed struct and handles all four
knobs uniformly.)*

`StorageProfile` is a `clap::ValueEnum` (`Ssd`/`Hdd`/`Nfs`) with a
`fn tunables(self) -> ProfileTunables` lookup returning the four values. `MountArgs`
construction in tests that build the struct directly (e.g. `cli.rs`) sets the new
`Option` fields to `None` unless exercising a specific value.

## Code changes

- **`musefs-cli/src/lib.rs`**
  - Add `pub enum StorageProfile { Ssd, Hdd, Nfs }` (`clap::ValueEnum`) and
    `struct ProfileTunables { max_readahead_kib, max_background, attr_ttl_ms,
    keep_cache }` with `StorageProfile::tunables(self)`.
  - Add named default constants; the `ssd` profile is built from them.
  - Add `#[arg(long, value_enum)] pub storage_profile: Option<StorageProfile>` to
    `MountArgs`.
  - Change `max_readahead_kib: u32`, `max_background: u16`, `attr_ttl_ms: u64` to
    `Option<…>` (drop their `default_value_t`); change `keep_cache: bool` to
    `Option<bool>` with `action = clap::ArgAction::Set` (drop the `store_true`
    presence form — see the keep-cache exception above).
  - Add `resolve_tunables` and call it where `MountConfig` / `FuseConfig` are built
    (the existing `max_readahead: args.max_readahead_kib.saturating_mul(1024)` site
    consumes the resolved value).

- **`musefs/Cargo.toml`** (binary crate) — add a `metrics` feature that forwards to
  `musefs-fuse/metrics` (which already forwards to `musefs-core/metrics`). This is
  **required by the validation harness**: the per-syscall fault hooks in
  `musefs-core/src/metrics.rs` compile to no-op stubs unless the `metrics` feature is
  on (`metrics.rs:242-246`), and the shipped binary does not enable it — so the
  harness must build the served binary with `cargo build -p musefs --features metrics`
  for `MUSEFS_FAULT_*_US` to take effect. The default release binary is unchanged
  (feature off, zero overhead).

No changes to `musefs-fuse`/`musefs-core` source or the DB layer — the resolved
values feed the existing `FuseConfig` fields unchanged, and the `metrics` feature
plumbing already exists in those crates' manifests.

## Validation methodology

**Why the existing benches can't measure this.** `read_throughput` (the Criterion
bench) and `bench_read_under_latency` both call `Musefs::read` / `read_at`
**in-process**, bypassing the kernel. The four tunables are kernel↔FUSE parameters
(read-ahead batching, background-queue depth, attr-cache TTL, page-cache retention),
so they are invisible to an in-process driver. Validation needs a **real kernel mount
with a real reader**, modeled on `benches/passthrough_dd.sh`.

**Latency injection — single FUSE layer, faults live in the mounted daemon.** Rather
than stack `musefs-latencyfs` under musefs (two real FUSE mounts, two CAP_SYS_ADMIN
setups), inject per-syscall storage latency into musefs's own serve path via the
existing env hooks in `musefs-core/src/metrics.rs`: `MUSEFS_FAULT_PREAD_US`,
`MUSEFS_FAULT_OPEN_US`, `MUSEFS_FAULT_STAT_US`, set to each medium's row from the
latency table.

**Critical prerequisite — the binary must be built `--features metrics`.** Those fault
hooks are no-op stubs (`metrics.rs:242-246`) unless the `metrics` feature is enabled,
and the shipped binary does **not** enable it. So the harness builds and mounts
`cargo build -p musefs --features metrics` (the new feature passthrough above); a
default-built binary would silently inject zero latency and the whole run would be
meaningless. The harness asserts the feature is on by sanity-checking that a known
fault actually slows a read before trusting any numbers.

With faults live, larger read-ahead batches the kernel's reads into fewer, larger
`pread`s to the daemon — each delayed once by the injected `PREAD` fault — so the
throughput delta isolates the read-ahead effect with one mount.

**Measurement is wall-clock, from outside the daemon.** The `metrics::snapshot`
counters (`preads`/`opens`) live *inside the mounted daemon process*; the harness is a
separate shell process driving the mount and cannot read them (the in-process
`RunReport`/`snapshot` mechanism in `bench_ingest.rs` only works because that bench
runs the FS in-process). The harness therefore measures **wall-clock only**: `dd`'s
own throughput line and `time` on the metadata walk. (Optional, deferred: have the
daemon log its `snapshot` on SIGTERM so counters can corroborate wall-clock; not
required for the pass criterion.)

**Serving mode.** Mount **StructureOnly** for the throughput driver (raw backing-byte
passthrough, like `passthrough_dd.sh`): format-independent, and it isolates the
backing-read path that read-ahead governs. Synthesis mode only prepends a small
synthesized header before the same `BackingAudio` reads, so it would not change the
read-ahead conclusion while adding a format/encoder dependency.

**Harness** — a new committed script `benches/storage_profile_bench.sh` (runnable,
in-tree, per project convention; large corpora stay gitignored):

1. Build the binary `--features metrics`; build the corpus + a large backing track
   (reuse the WAV generator from `passthrough_dd.sh`); scan into a DB.
2. For each latency setting in {ssd, hdd, nfs-ssd, nfs-hdd} (the four `Latency`
   rows), and for each config in {current-defaults, candidate-profile}:
   - mount musefs (StructureOnly) with `MUSEFS_FAULT_{PREAD,OPEN,STAT}_US` set from
     the row and the config's resolved tunables;
   - **sequential throughput** (exercises `max_readahead` / `keep_cache`):
     `dd if=<virt> of=/dev/null bs=1M`, cold then warm, 3 reps; record the dd
     throughput line. `keep_cache` shows up as the warm-rep delta (page cache retained
     across the re-open vs re-fetched under the `PREAD` fault).
   - **concurrent throughput** (exercises `max_background`): N parallel
     `dd`/`cat` streams over **distinct** tracks (a single sequential stream will not
     fill the background queue, so `max_background` is invisible to the
     single-stream driver); record aggregate wall time. This needs a multi-track
     corpus, not the single large track.
   - **metadata walk** (exercises `attr_ttl_ms`): `time` a `find <mnt> -type f`
     + `stat` pass, repeated twice with `MUSEFS_FAULT_STAT_US`/`_OPEN_US` set high;
     the second pass should hit the attr cache when TTL is high. **Caveat:** under a
     single mount the attr-cache signal is weak and noisy; if it does not separate
     cleanly, `attr_ttl_ms` is documented as *reasoned, not bench-proven*, and the
     run says so rather than reporting a spurious win.
3. **Pass criterion.** "Tie within noise" = the candidate's median is within the
   run-to-run spread of the defaults (report min/median/max of the 3 reps; treat a
   difference smaller than the defaults' own max−min as a tie). For each medium the
   candidate profile must **beat** the defaults on that medium's decisive metric
   (sequential throughput for hdd/nfs read-ahead; concurrent throughput for nfs
   `max_background`) or, where the signal is a tie, fall back toward the default and
   record the value as reasoned. ssd must not regress on any metric.
4. Record the before/after table in `BENCHMARKS.md` (per the
   [SP validation convention](../../../BENCHMARKS.md)); the values that land in the
   README/code table are exactly the confirmed ones.

The script needs `/dev/fuse` and a metrics-built binary; it is a sudo/local run, not
CI — same tier as the existing `--ignored` latency benches.

## Testing

- **`musefs-cli/tests/cli.rs`** (mirrors the existing `parse_mount_config_*` tests).
  Every `MountArgs` struct-literal construction site in this file (the existing
  `parse_mount_config_*` / `saturating_readahead` tests build the struct directly,
  ~4 sites) must be updated for the new field shapes: `storage_profile: None`, the
  three numeric knobs as `None`, and `keep_cache: None`. New/updated cases:
  - `storage_profile_hdd_sets_tunables` — `--storage-profile hdd` yields the hdd
    `FuseConfig` values.
  - `explicit_flag_overrides_profile` — `--storage-profile nfs --max-readahead-kib
    4096 --keep-cache false` keeps nfs `max_background`/`attr_ttl` but uses the
    explicit read-ahead and the explicit `keep_cache=false` (proves the
    override-back-to-off path the value-form flag exists for).
  - `no_profile_keeps_defaults` — bare `mount` still yields today's defaults
    (regression guard for backwards compatibility).
  - `keep_cache_flag_requires_value` — bare `--keep-cache` now errors (UX-change
    guard); `--keep-cache true`/`false` parse to `Some(true)`/`Some(false)`.
  - `saturating_readahead` and the existing default assertions updated for the new
    `Option` fields / resolution path.
- Unit test for `StorageProfile::tunables` covering all three variants (cheap table
  guard so a typo in a value is caught without a mount).

## Documentation

- **README** mount-options table: add a `--storage-profile` row and a short
  "pick by backing store" subsection containing the confirmed values table and the
  one-line rationale per knob. Note the precedence rule (explicit flag wins) and the
  attr-ttl visibility tradeoff. Update the `--keep-cache` row and any examples to the
  value form (`--keep-cache true|false`); bare `--keep-cache` no longer parses.
- Doc comment on the new `storage_profile` field and a one-line note on each of the
  four flags that they fall back to the profile / built-in default when unset.

## Migration note (the one user-visible breaking change)

The `--keep-cache` change from presence-flag to value-flag is a breaking CLI change
for anyone passing bare `--keep-cache`. It is called out here, in the README, and in
the commit message; the implementation plan's first code commit carries the test
(`keep_cache_flag_requires_value`) that pins the new behavior. No other existing
command line changes meaning.

## Out of scope

- Auto-detecting the backing filesystem type (`statfs` f_type) to pick a profile —
  possible future ergonomic win, but it guesses where the user knows, and NFS-over-X
  ambiguity remains. Deferred.
- Per-format or per-track profiles. The medium is a mount-wide property.
- Tuning `--poll-interval-ms` per medium; it governs the DB-watch debounce, not the
  backing read path, so it is out of the media-profile scope.
