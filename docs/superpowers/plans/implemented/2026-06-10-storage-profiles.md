# Storage-aware mount profiles (`--storage-profile`) Implementation Plan

> **Outcome: reverted.** Tasks 1–3 were implemented, but Task 4 (bench validation)
> disproved the premise: only `--keep-cache` helps on slow backing; the
> `max_readahead`/`max_background` bumps give no benefit (read-ahead even hurts on HDD).
> The CLI feature was reverted and the preset dropped. Evidence:
> [BENCHMARKS.md §Storage tunables](../../../BENCHMARKS.md#storage-tunables). Retained
> for historical context.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `--storage-profile {ssd,hdd,nfs}` mount preset that sets the four backing-media tunables (`max-readahead-kib`, `max-background`, `attr-ttl-ms`, `keep-cache`) to bench-validated values, with explicit flags overriding the profile.

**Architecture:** Additive CLI layer in `musefs-cli`. The four media flags become `Option<…>` (unset = "take from profile or built-in default"); a pure `resolve_tunables` step folds `(explicit flag, profile, default)` into the effective values that feed the existing `FuseConfig`. No changes to `musefs-fuse`/`musefs-core` serving code. Profile values are first implemented as a hypothesis, then confirmed/adjusted by a kernel-mount latency bench before they ship.

**Tech Stack:** Rust, `clap` v4 derive, the existing `musefs-core::metrics` fault-injection hooks, a shell bench harness modeled on `benches/passthrough_dd.sh`.

**Spec:** `docs/superpowers/specs/2026-06-10-storage-profiles-design.md`

---

## File structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs/Cargo.toml` | binary crate manifest | add `[features] metrics` passthrough |
| `musefs-cli/Cargo.toml` | cli crate manifest | add `[features] metrics` passthrough |
| `musefs-cli/src/lib.rs` | CLI parsing + `parse_mount_config` | add `StorageProfile`, constants, `resolve_tunables`; `Option`-ize four fields; wire resolution |
| `musefs-cli/tests/cli.rs` | CLI integration tests | update 4 construction sites + default assertions; add profile/override/keep-cache tests |
| `benches/storage_profile_bench.sh` | kernel-mount latency bench harness | new file |
| `BENCHMARKS.md` (repo root) | bench results log | append the storage-profile before/after table |
| `README.md` | user docs | update Tuning table; add "pick by backing store" subsection |

---

## Task 1: `metrics` feature passthrough (binary + cli crate)

The bench harness in Task 3 injects latency via `MUSEFS_FAULT_*_US`, but those hooks
compile to no-op stubs (`musefs-core/src/metrics.rs:242-246`) unless the `metrics`
feature is on, and the shipped binary leaves it off. The binary depends on
`musefs-cli` (not `musefs-fuse` directly), so the feature must chain
`musefs` → `musefs-cli` → `musefs-fuse` → `musefs-core` (the last two links already
exist in their manifests).

**Files:**
- Modify: `musefs-cli/Cargo.toml`
- Modify: `musefs/Cargo.toml`

- [ ] **Step 1: Add the `metrics` feature to `musefs-cli/Cargo.toml`**

Insert after the `[dependencies]` block (after line 17, before `[dev-dependencies]`):

```toml
[features]
# Forwards to musefs-fuse/metrics (which forwards to musefs-core/metrics), enabling
# the serve-path counters and MUSEFS_FAULT_*_US latency injection. Off by default.
metrics = ["musefs-fuse/metrics"]
```

- [ ] **Step 2: Add the `metrics` feature to `musefs/Cargo.toml`**

Insert after the `[dependencies]` block (after line 19, before `[dev-dependencies]`):

```toml
[features]
# Build with `--features metrics` to enable serve-path latency injection
# (used by benches/storage_profile_bench.sh). Off in release builds.
metrics = ["musefs-cli/metrics"]
```

- [ ] **Step 3: Verify the default build is unchanged and the feature build compiles**

Run: `cargo build -p musefs && cargo build -p musefs --features metrics`
Expected: both succeed.

- [ ] **Step 4: Verify the feature actually reaches `musefs-core`**

Run: `cargo tree -p musefs --features metrics -e features -i musefs-core | grep -i metrics`
Expected: output shows `musefs-core` is built with its `metrics` feature (a line containing `feature "metrics"`). If empty, the chain is broken — recheck Steps 1-2.

- [ ] **Step 5: Commit**

```bash
git add musefs-cli/Cargo.toml musefs/Cargo.toml
git commit -m "build: add metrics feature passthrough for the latency bench

The musefs-core fault-injection hooks (MUSEFS_FAULT_*_US) are no-op stubs
unless the metrics feature is on. Chain it musefs -> musefs-cli ->
musefs-fuse -> musefs-core so benches/storage_profile_bench.sh can build a
latency-injecting binary. Release builds are unaffected (feature off)."
```

---

## Task 2: CLI wiring — profile enum, `Option` fields, and `resolve_tunables`

This is the feature's core and must land as **one green commit** (the field type
change breaks compilation of `parse_mount_config` and the tests until all are updated;
the pre-commit hook runs the full test suite + `clippy -D warnings`, so an unused enum
would also fail). Write the tests first, then implement, then make it all green.

Profile values below are the **hypothesis** from the spec — Task 4 confirms or adjusts
them.

**Files:**
- Modify: `musefs-cli/src/lib.rs` (the `MountArgs` struct lines 43-91; `parse_mount_config` lines 190-206; add a `#[cfg(test)]` unit-test module)
- Test: `musefs-cli/tests/cli.rs`

- [ ] **Step 1: Add the profile types, constants, and resolver to `musefs-cli/src/lib.rs`**

Add this block immediately **above** the `MountArgs` struct definition (currently the
doc comment `/// Flags for \`musefs mount\`…` at line 43). It defines the enum (must be
`pub` — it appears in a `pub` struct field), the default constants, the per-profile
table, and the pure resolver:

```rust
/// Built-in tunable defaults — used when neither `--storage-profile` nor the
/// explicit flag is given. The `ssd` profile is intentionally identical to these.
const DEFAULT_MAX_READAHEAD_KIB: u32 = 512;
const DEFAULT_MAX_BACKGROUND: u16 = 64;
const DEFAULT_ATTR_TTL_MS: u64 = 1000;
const DEFAULT_KEEP_CACHE: bool = false;

/// The four backing-media tunables, resolved to concrete values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProfileTunables {
    max_readahead_kib: u32,
    max_background: u16,
    attr_ttl_ms: u64,
    keep_cache: bool,
}

impl ProfileTunables {
    /// The built-in defaults (== the `ssd` profile).
    const DEFAULTS: ProfileTunables = ProfileTunables {
        max_readahead_kib: DEFAULT_MAX_READAHEAD_KIB,
        max_background: DEFAULT_MAX_BACKGROUND,
        attr_ttl_ms: DEFAULT_ATTR_TTL_MS,
        keep_cache: DEFAULT_KEEP_CACHE,
    };
}

/// Recommended tunable preset for a backing medium. Sets all four media flags at
/// once; an explicitly-passed flag still overrides the profile for that one knob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum StorageProfile {
    /// Local SSD/NVMe — seek-free. Equivalent to the built-in defaults.
    Ssd,
    /// Local spinning disk — large read-ahead amortizes the ~8 ms seek; page cache
    /// kept to avoid re-seeking on repeat opens.
    Hdd,
    /// Network filesystem (NFS/SMB) — large read-ahead and a deeper background queue
    /// hide the per-RPC tax; a longer attr TTL cuts metadata RPCs.
    Nfs,
}

impl StorageProfile {
    fn tunables(self) -> ProfileTunables {
        match self {
            // ssd MUST equal DEFAULTS (see the unit test that pins this).
            StorageProfile::Ssd => ProfileTunables::DEFAULTS,
            StorageProfile::Hdd => ProfileTunables {
                max_readahead_kib: 2048,
                max_background: 64,
                attr_ttl_ms: 2000,
                keep_cache: true,
            },
            StorageProfile::Nfs => ProfileTunables {
                max_readahead_kib: 2048,
                max_background: 128,
                attr_ttl_ms: 5000,
                keep_cache: true,
            },
        }
    }
}

/// Fold `(explicit flag, profile, built-in default)` into the effective tunables.
/// Precedence: explicit flag > profile > default.
fn resolve_tunables(args: &MountArgs) -> ProfileTunables {
    let base = args
        .storage_profile
        .map_or(ProfileTunables::DEFAULTS, StorageProfile::tunables);
    ProfileTunables {
        max_readahead_kib: args.max_readahead_kib.unwrap_or(base.max_readahead_kib),
        max_background: args.max_background.unwrap_or(base.max_background),
        attr_ttl_ms: args.attr_ttl_ms.unwrap_or(base.attr_ttl_ms),
        keep_cache: args.keep_cache.unwrap_or(base.keep_cache),
    }
}
```

- [ ] **Step 2: Change the four `MountArgs` fields to `Option` and add `storage_profile`**

In `MountArgs`, replace the four media-flag fields (currently lines ~73-89: the
`max_readahead_kib`, `max_background`, `attr_ttl_ms`, and `keep_cache` fields with
their `default_value_t` / bare `#[arg(long)]` forms) with the versions below, and add
the new `storage_profile` field. Leave `poll_interval_ms` and `case_insensitive`
untouched.

```rust
    /// Preset tunables for the backing medium. Sets read-ahead, background queue,
    /// attr TTL, and page-cache retention at once. An explicitly-passed flag below
    /// overrides the profile for that knob. Unset = built-in defaults.
    #[arg(long, value_enum)]
    pub storage_profile: Option<StorageProfile>,
    /// Entry/attr cache TTL (ms) the kernel may trust before re-validating. Higher
    /// cuts lookup/getattr traffic but slows visibility of DB edits. Unset = profile
    /// value, else 1000.
    #[arg(long)]
    pub attr_ttl_ms: Option<u64>,
    /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while streaming;
    /// clamped to the kernel maximum at mount. Unset = profile value, else 512.
    #[arg(long)]
    pub max_readahead_kib: Option<u32>,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    /// Unset = profile value, else 64.
    #[arg(long)]
    pub max_background: Option<u16>,
    /// Keep the kernel page cache across opens. External re-tags auto-invalidate the
    /// affected inodes on refresh, so cached bytes are dropped when content changes.
    /// Takes a value (`--keep-cache true|false`); unset = profile value, else false.
    #[arg(long, action = clap::ArgAction::Set)]
    pub keep_cache: Option<bool>,
```

- [ ] **Step 3: Rewrite `parse_mount_config` to use the resolver**

Replace the `fuse_config` construction inside `parse_mount_config`
(`musefs-cli/src/lib.rs:199-204`) so it folds through `resolve_tunables`. The
`config` (MountConfig) block above it is unchanged.

```rust
    let t = resolve_tunables(args);
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(t.attr_ttl_ms),
        max_readahead: t.max_readahead_kib.saturating_mul(1024),
        max_background: t.max_background,
        keep_cache: t.keep_cache,
    };
```

- [ ] **Step 4: Add the `tunables` table unit test (pins "ssd == defaults")**

Append a `#[cfg(test)]` module at the end of `musefs-cli/src/lib.rs` (this lives in the
crate, so it can see the private `ProfileTunables`/`tunables`):

```rust
#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn ssd_profile_equals_builtin_defaults() {
        assert_eq!(StorageProfile::Ssd.tunables(), ProfileTunables::DEFAULTS);
    }

    #[test]
    fn hdd_and_nfs_tunables_are_as_specified() {
        let hdd = StorageProfile::Hdd.tunables();
        assert_eq!(hdd.max_readahead_kib, 2048);
        assert_eq!(hdd.max_background, 64);
        assert_eq!(hdd.attr_ttl_ms, 2000);
        assert!(hdd.keep_cache);

        let nfs = StorageProfile::Nfs.tunables();
        assert_eq!(nfs.max_readahead_kib, 2048);
        assert_eq!(nfs.max_background, 128);
        assert_eq!(nfs.attr_ttl_ms, 5000);
        assert!(nfs.keep_cache);
    }
}
```

- [ ] **Step 5: Update the default-and-explicit parse assertions in `cli.rs`**

In `musefs-cli/tests/cli.rs`, the `parses_mode_and_revalidate_flags` test asserts the
old literal defaults and uses bare `--keep-cache`. Replace its "defaults" block
(currently lines 54-65) so unset flags are `None`:

```rust
    // Mode defaults to synthesis; media tuning flags are unset (resolved later).
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount(args) => {
            assert_eq!(args.mode, CliMode::Synthesis);
            assert_eq!(args.poll_interval_ms, 1000); // default (unchanged)
            assert_eq!(args.storage_profile, None);
            assert_eq!(args.attr_ttl_ms, None);
            assert_eq!(args.max_readahead_kib, None);
            assert_eq!(args.max_background, None);
            assert_eq!(args.keep_cache, None);
        }
        Command::Scan { .. } => panic!("expected mount"),
    }
```

Replace its "explicit values" block (currently lines 67-93) — note `--keep-cache` now
takes a value:

```rust
    // Tuning flags parse to their given values.
    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--poll-interval-ms",
        "500",
        "--attr-ttl-ms",
        "2000",
        "--max-readahead-kib",
        "1024",
        "--max-background",
        "128",
        "--keep-cache",
        "true",
    ]);
    match cli.command {
        Command::Mount(args) => {
            assert_eq!(args.poll_interval_ms, 500);
            assert_eq!(args.attr_ttl_ms, Some(2000));
            assert_eq!(args.max_readahead_kib, Some(1024));
            assert_eq!(args.max_background, Some(128));
            assert_eq!(args.keep_cache, Some(true));
        }
        Command::Scan { .. } => panic!("expected mount"),
    }
```

- [ ] **Step 6: Update the four `MountArgs` struct-literal construction sites in `cli.rs`**

Four tests build `MountArgs` directly and must set the new field shapes. Apply each
replacement (the surrounding `let (config, fuse_config) = parse_mount_config(&args);`
and assertions below them stay as-is unless noted).

`parse_mount_config_defaults_are_sensible` (lines 121-134) — all media flags `None`,
asserting the resolver produces the defaults:

```rust
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$artist/$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![],
        mode: musefs_cli::CliMode::Synthesis,
        poll_interval_ms: 1000,
        storage_profile: None,
        attr_ttl_ms: None,
        max_readahead_kib: None,
        max_background: None,
        keep_cache: None,
        case_insensitive: false,
    };
```

`parse_mount_config_keep_cache_sets_flag` (lines 149-162):

```rust
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![],
        mode: musefs_cli::CliMode::StructureOnly,
        poll_interval_ms: 250,
        storage_profile: None,
        attr_ttl_ms: Some(5000),
        max_readahead_kib: Some(256),
        max_background: Some(32),
        keep_cache: Some(true),
        case_insensitive: false,
    };
```

`parse_mount_config_saturating_readahead` (lines 173-186):

```rust
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![],
        mode: musefs_cli::CliMode::Synthesis,
        poll_interval_ms: 1000,
        storage_profile: None,
        attr_ttl_ms: None,
        max_readahead_kib: Some(u32::MAX),
        max_background: None,
        keep_cache: None,
        case_insensitive: false,
    };
```

`parse_mount_config_populates_per_field_fallbacks` (lines 218-234):

```rust
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$albumartist/$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![
            ("albumartist".to_string(), "Unknown Artist".to_string()),
            ("genre".to_string(), "Misc".to_string()),
        ],
        mode: musefs_cli::CliMode::Synthesis,
        poll_interval_ms: 1000,
        storage_profile: None,
        attr_ttl_ms: None,
        max_readahead_kib: None,
        max_background: None,
        keep_cache: None,
        case_insensitive: false,
    };
```

- [ ] **Step 7: Add the profile / override / keep-cache tests to `cli.rs`**

Append these four tests at the end of `musefs-cli/tests/cli.rs` (they reuse the
existing `use musefs_cli::parse_mount_config;` and `use std::time::Duration;` imports
at lines 115-117):

```rust
#[test]
fn storage_profile_hdd_sets_tunables() {
    let cli = Cli::parse_from([
        "musefs", "mount", "/mnt/x", "--db", "/tmp/m.db",
        "--storage-profile", "hdd",
    ]);
    let args = match cli.command {
        Command::Mount(args) => args,
        Command::Scan { .. } => panic!("expected mount"),
    };
    let (_, fuse_config) = parse_mount_config(&args);
    assert_eq!(fuse_config.max_readahead, 2048 * 1024);
    assert_eq!(fuse_config.max_background, 64);
    assert_eq!(fuse_config.ttl, Duration::from_millis(2000));
    assert!(fuse_config.keep_cache);
}

#[test]
fn explicit_flag_overrides_profile() {
    let cli = Cli::parse_from([
        "musefs", "mount", "/mnt/x", "--db", "/tmp/m.db",
        "--storage-profile", "nfs",
        "--max-readahead-kib", "4096",
        "--keep-cache", "false",
    ]);
    let args = match cli.command {
        Command::Mount(args) => args,
        Command::Scan { .. } => panic!("expected mount"),
    };
    let (_, fuse_config) = parse_mount_config(&args);
    // Explicit flags win:
    assert_eq!(fuse_config.max_readahead, 4096 * 1024);
    assert!(!fuse_config.keep_cache);
    // Untouched nfs-profile values remain:
    assert_eq!(fuse_config.max_background, 128);
    assert_eq!(fuse_config.ttl, Duration::from_millis(5000));
}

#[test]
fn no_profile_keeps_defaults() {
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    let args = match cli.command {
        Command::Mount(args) => args,
        Command::Scan { .. } => panic!("expected mount"),
    };
    let (_, fuse_config) = parse_mount_config(&args);
    assert_eq!(fuse_config.max_readahead, 512 * 1024);
    assert_eq!(fuse_config.max_background, 64);
    assert_eq!(fuse_config.ttl, Duration::from_secs(1));
    assert!(!fuse_config.keep_cache);
}

#[test]
fn keep_cache_flag_requires_value() {
    // Bare `--keep-cache` no longer parses (UX change from presence to value flag).
    let err = Cli::try_parse_from([
        "musefs", "mount", "/mnt/x", "--db", "/tmp/m.db", "--keep-cache",
    ])
    .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("value"),
        "expected a missing-value error, got: {err}"
    );

    for (arg, want) in [("true", true), ("false", false)] {
        let cli = Cli::parse_from([
            "musefs", "mount", "/mnt/x", "--db", "/tmp/m.db", "--keep-cache", arg,
        ]);
        match cli.command {
            Command::Mount(args) => assert_eq!(args.keep_cache, Some(want)),
            Command::Scan { .. } => panic!("expected mount"),
        }
    }
}
```

- [ ] **Step 8: Build, lint, and run the full cli test suite**

Run: `cargo test -p musefs-cli && cargo clippy -p musefs-cli --all-targets -- -D warnings`
Expected: all tests pass; clippy clean (no `dead_code` on the new enum/resolver because
`parse_mount_config` and the tests use them).

- [ ] **Step 9: Run the full workspace test suite (pre-commit parity)**

Run: `cargo test`
Expected: PASS (the pre-commit hook runs this; a red commit is rejected).

- [ ] **Step 10: Commit**

```bash
git add musefs-cli/src/lib.rs musefs-cli/tests/cli.rs
git commit -m "feat(cli): add --storage-profile {ssd,hdd,nfs} preset

Preset the four backing-media tunables per medium. The flags become
Option (unset = take from profile, else built-in default); an explicit
flag overrides the profile. --keep-cache becomes a value flag
(--keep-cache true|false) so a profile's keep_cache can be overridden
off, matching the existing --case-insensitive style. Profile values are
the spec hypothesis; Task 4 confirms them via bench."
```

---

## Task 3: Kernel-mount latency bench harness

A committed, runnable shell harness (per the in-tree-harness convention). It must pass
`shellcheck` (the pre-commit hook lints tracked shell files). It mounts a
`--features metrics` binary, injects per-syscall latency, and compares the candidate
profile against current defaults under each latency profile. Measurement is
**wall-clock from outside the daemon** — the `metrics::snapshot` counters live inside
the mounted process and a shell can't read them.

**Files:**
- Create: `benches/storage_profile_bench.sh`

- [ ] **Step 1: Write the harness script**

Create `benches/storage_profile_bench.sh` with exactly this content:

```bash
#!/usr/bin/env bash
# Storage-profile validation bench.
# Mounts musefs (built --features metrics) over a corpus with per-syscall storage
# latency injected via MUSEFS_FAULT_*_US, and compares each candidate --storage-profile
# against the current built-in defaults under each latency profile. Wall-clock only:
# the metrics::snapshot counters live inside the daemon and are not readable here.
#
# Usage: sudo benches/storage_profile_bench.sh <work-dir> [size-mib] [streams]
set -euo pipefail
WORK="${1:?usage: sudo $0 <work-dir> [size-mib] [streams]}"
SIZE_MIB="${2:-512}"
# Concurrent streams for the max_background driver. A 64-vs-128 background-queue
# difference only separates with many in-flight requests, so default high; even so it
# may register as a tie (see the bench task — nfs max_background is then kept-as-reasoned).
STREAMS="${3:-32}"
ROOT="$(git rev-parse --show-toplevel)"
BIN="$ROOT/target/release/musefs"

if [ ! -x "$BIN" ]; then
  echo "ERROR: $BIN not found. Build it first:" >&2
  echo "  cargo build --release -p musefs --features metrics" >&2
  exit 1
fi

mkdir -p "$WORK/backing" "$WORK/mnt"

# The injected MUSEFS_FAULT_*_US latency must be the ONLY latency in the read path, so
# the backing corpus must live on a RAM-backed filesystem (tmpfs/ramfs). On real disk,
# the disk's own seek latency stacks on top of the injection, and the ssd row (which
# injects ZERO latency) would measure the disk instead of RAM — destroying the
# "ssd must not regress" baseline. Refuse to run on anything else.
fstype="$(stat -f -c %T "$WORK")"
case "$fstype" in
  tmpfs|ramfs) ;;
  *)
    echo "ERROR: work-dir '$WORK' is on '$fstype', not tmpfs/ramfs." >&2
    echo "       Use a RAM-backed dir (e.g. /dev/shm/musefs-spbench or /tmp/musefs-spbench)" >&2
    echo "       so only the injected latency is measured. On this box /home and /data are HDD." >&2
    exit 1
    ;;
esac

# One large WAV (sequential-throughput driver) + a few smaller tracks
# (concurrent-stream and metadata-walk drivers). Reuses passthrough_dd.sh's header.
make_wav() {
  local path="$1" mib="$2" data riff
  [ -f "$path" ] && return 0
  data=$(( mib * 1024 * 1024 )); riff=$(( data + 36 ))
  # shellcheck disable=SC2059  # generated hex-escape format string, by design
  {
    printf 'RIFF'
    printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((riff&255)) $((riff>>8&255)) $((riff>>16&255)) $((riff>>24&255)))"
    printf 'WAVEfmt '
    printf '\x10\x00\x00\x00\x01\x00\x02\x00\x44\xac\x00\x00\x10\xb1\x02\x00\x04\x00\x10\x00'
    printf 'data'
    printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((data&255)) $((data>>8&255)) $((data>>16&255)) $((data>>24&255)))"
    dd if=/dev/zero bs=1M count="$mib" 2>/dev/null
  } >> "$path"
}

make_wav "$WORK/backing/big.wav" "$SIZE_MIB"
for i in $(seq 1 "$STREAMS"); do make_wav "$WORK/backing/t$i.wav" 64; done

DB="$WORK/m.db"; rm -f "$DB"
"$BIN" scan "$WORK/backing" --db "$DB" >/dev/null

# Latency rows (microseconds) mirroring musefs-latencyfs Latency::profile:
#   name      pread  open  stat   matching-candidate-profile
ROWS=(
  "ssd      0     0     0     ssd"
  "hdd      8000  8000  8000  hdd"
  "nfs-ssd  600   600   400   nfs"
  "nfs-hdd  8600  8600  8400  nfs"
)

mount_musefs() {
  # $1=pread_us $2=open_us $3=stat_us ; remaining args = extra musefs flags
  local pread="$1" open="$2" stat="$3"; shift 3
  # shellcheck disable=SC2016  # '$title' is a musefs output-template literal, not a shell var
  MUSEFS_FAULT_PREAD_US="$pread" MUSEFS_FAULT_OPEN_US="$open" MUSEFS_FAULT_STAT_US="$stat" \
    "$BIN" mount "$WORK/mnt" --db "$DB" --mode structure-only --template '$title' "$@" &
  MPID=$!
  local virt=""
  for _ in $(seq 1 60); do
    virt=$(find "$WORK/mnt" -type f 2>/dev/null | head -1 || true)
    [ -n "$virt" ] && break
    sleep 0.5
  done
  if [ -z "$virt" ]; then echo "ERROR: mount never exposed a file" >&2; kill "$MPID" 2>/dev/null || true; exit 1; fi
}

unmount_musefs() {
  fusermount3 -u "$WORK/mnt" 2>/dev/null || umount "$WORK/mnt" 2>/dev/null || true
  kill "$MPID" 2>/dev/null || true
  wait "$MPID" 2>/dev/null || true
}

# Virtual filenames are tag-based (untagged WAVs collide and get disambiguated by
# musefs), so identify tracks by SIZE, not name. The big track is the largest file.
biggest_virt() { find "$WORK/mnt" -type f -printf '%s\t%p\n' | sort -rn | head -1 | cut -f2-; }
small_virts()  { find "$WORK/mnt" -type f -printf '%s\t%p\n' | sort -rn | tail -n +2 | cut -f2-; }

# Median of 3 `dd` runs (MB/s) reading the big track sequentially.
seq_mbps() {
  local virt; virt="$(biggest_virt)"
  local out
  out=$(for _ in 1 2 3; do dd if="$virt" of=/dev/null bs=1M 2>&1 | tail -1 | grep -oE '[0-9.]+ MB/s' | grep -oE '[0-9.]+'; done | sort -n | sed -n 2p)
  echo "${out:-0}"
}

# Wall-clock seconds for parallel reads over the distinct smaller tracks.
concurrent_secs() {
  local files; mapfile -t files < <(small_virts)
  local t0 t1
  t0=$(date +%s.%N)
  for f in "${files[@]}"; do cat "$f" > /dev/null & done
  wait
  t1=$(date +%s.%N)
  awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}'
}

# Self-check: with a large PREAD fault, a sequential read must be visibly slow.
# If not, the binary was built WITHOUT --features metrics and every number is bogus.
echo "== fault-injection self-check =="
mount_musefs 50000 0 0
SLOW=$(seq_mbps)
unmount_musefs
mount_musefs 0 0 0
FAST=$(seq_mbps)
unmount_musefs
echo "  50ms-pread: ${SLOW} MB/s   0-pread: ${FAST} MB/s"
awk -v s="$SLOW" -v f="$FAST" 'BEGIN{ if (s+0 >= f+0) { print "ERROR: fault not active — rebuild with: cargo build --release -p musefs --features metrics" > "/dev/stderr"; exit 1 } }'

printf '\n%-9s %-9s %12s %12s %14s %14s\n' profile latency def_MBps prof_MBps def_conc_s prof_conc_s
for row in "${ROWS[@]}"; do
  read -r name pread open stat cand <<<"$row"
  # candidate profile tunables come from --storage-profile; defaults from no profile
  mount_musefs "$pread" "$open" "$stat"
  d_mbps=$(seq_mbps); d_conc=$(concurrent_secs)
  unmount_musefs
  mount_musefs "$pread" "$open" "$stat" --storage-profile "$cand"
  p_mbps=$(seq_mbps); p_conc=$(concurrent_secs)
  unmount_musefs
  printf '%-9s %-9s %12s %12s %14s %14s\n' "$cand" "$name" "$d_mbps" "$p_mbps" "$d_conc" "$p_conc"
done
echo
echo "Higher MB/s and lower concurrent-seconds are better. Record the table in BENCHMARKS.md."
```

- [ ] **Step 2: Make it executable and shellcheck-clean**

Run: `chmod +x benches/storage_profile_bench.sh && shellcheck benches/storage_profile_bench.sh`
Expected: **exit 0, no output.** Two `# shellcheck disable=` lines are intentional and
match `passthrough_dd.sh`: `SC2059` (the generated hex-escape `printf` in the WAV
header) and `SC2016` (the single-quoted `--template '$title'` literal). Note the
pre-commit hook treats *any* non-zero shellcheck exit — including info-level findings
like SC2016 — as a failure, so both disables must be present. Fix any other finding
before committing.

- [ ] **Step 3: Commit**

```bash
git add benches/storage_profile_bench.sh
git commit -m "test(bench): add storage-profile latency validation harness

Mounts a --features metrics binary, injects per-syscall latency via
MUSEFS_FAULT_*_US, and compares each --storage-profile against defaults
under ssd/hdd/nfs-ssd/nfs-hdd latency. Includes a self-check that aborts
if the binary lacks the metrics feature (else it measures zero latency)."
```

---

## Task 4: Run the bench, record results, reconcile the profile values

This task has a **manual sudo run** (needs `/dev/fuse` + CAP_SYS_ADMIN; see the
`fuse-passthrough-cap-sys-admin` memory) and may edit the Task 2 constants if the data
moves them.

**Files:**
- Modify (maybe): `musefs-cli/src/lib.rs` (`StorageProfile::tunables` values + the unit test, if the bench moves them)
- Modify: `BENCHMARKS.md` (repo root)

- [ ] **Step 1: Build the metrics binary**

Run: `cargo build --release -p musefs --features metrics`
Expected: produces `target/release/musefs` with fault injection active.

- [ ] **Step 2: Run the bench (work-dir MUST be on tmpfs)**

The work-dir has to be RAM-backed so the injected latency is the only latency measured;
the script refuses to run elsewhere. On this box `/tmp` and `/dev/shm` are tmpfs while
`/home` (the worktree, RAID-1 HDD) and `/data` (HDD) are not — so do **not** point the
work-dir at the worktree. The corpus is ~2.5 GiB (512 MiB big track + 32×64 MiB
streams), which fits `/tmp` comfortably.

Run: `sudo benches/storage_profile_bench.sh /dev/shm/musefs-spbench 512 32`
Expected: the self-check passes (50ms-pread row is much slower than 0-pread), then a
table with `def_MBps`/`prof_MBps`/`def_conc_s`/`prof_conc_s` per latency profile. (The
build artifacts under the worktree's `target/` stay on HDD — that only affects build
time, not the measurement, since the binary is loaded into RAM once.)

- [ ] **Step 3: Evaluate against the pass criterion**

For each latency profile, check the candidate profile vs defaults on its decisive
metric (from the spec):
- **hdd / nfs-ssd / nfs-hdd:** `prof_MBps` should beat `def_MBps` — this is the
  read-ahead win and the primary, reliably-observable signal of this harness.
- **ssd:** must not regress on either metric.
- If a knob shows only a tie (candidate within the defaults' own run-to-run spread),
  keep the default value for that knob and note it as *reasoned, not bench-proven*.
  **Expect this for two knobs:** `attr_ttl_ms` (weak attr-cache signal under one mount)
  and the nfs `max_background` 64→128 bump (a 64-deep queue only separates with far
  more than 32 in-flight requests, which this single-host harness won't reliably
  generate). If `prof_conc_s` does not clearly beat `def_conc_s`, keep `max_background`
  at 128 on the strength of the latency-model reasoning and record it as reasoned —
  do not lower it to 64 just because the harness can't separate it.

- [ ] **Step 4: Reconcile the code if the data moved any value**

If the bench shows a different value wins (e.g. hdd readahead 4096 beats 2048), update
**all three** places that hardcode the value, keeping them in sync:
1. the corresponding arm of `StorageProfile::tunables` in `musefs-cli/src/lib.rs`;
2. the `hdd_and_nfs_tunables_are_as_specified` unit test in `musefs-cli/src/lib.rs`;
3. the matching assertions in the `cli.rs` integration tests —
   `storage_profile_hdd_sets_tunables` (hdd `max_readahead`/`max_background`/`ttl`/
   `keep_cache`) and `explicit_flag_overrides_profile` (the nfs `max_background == 128`
   and `ttl == 5000ms` it leaves untouched).

Then run: `cargo test -p musefs-cli`
Expected: PASS with the updated values.

If no value changed, skip the code edit.

- [ ] **Step 5: Record the results in `BENCHMARKS.md`**

Append a `## Storage profiles (2026-06-10)` section to the repo-root `BENCHMARKS.md`
containing: the machine/run context, the raw table from Step 2, and a one-line verdict
per profile (which knobs were bench-confirmed vs kept-as-reasoned). Match the existing
section formatting in that file.

- [ ] **Step 6: Commit**

```bash
# If Step 4 changed code, include the src + test files; always include BENCHMARKS.md.
git add BENCHMARKS.md musefs-cli/src/lib.rs musefs-cli/tests/cli.rs 2>/dev/null || git add BENCHMARKS.md
git commit -m "test(bench): record storage-profile validation results

Run the latency bench across ssd/hdd/nfs-ssd/nfs-hdd and record the
before/after table in BENCHMARKS.md. Profile values reconciled to the
confirmed numbers (knobs that only tied keep the default, noted as
reasoned)."
```

---

## Task 5: Documentation (README)

Docs-only commit (the cargo gate is skipped for `*.md` / `docs/` paths). Use the
**confirmed** values from Task 4 — if Task 4 changed any number, use that number here.

**Files:**
- Modify: `README.md` (Tuning section, lines 110-121)
- Modify (only if the sweep finds bare-invocation examples): other `docs/` / `*.md`

- [ ] **Step 0: Sweep for bare `--keep-cache` invocation examples**

The spec requires updating any docs/examples that *invoke* bare `--keep-cache` (now a
value flag).

Run: `grep -rn -- "--keep-cache" README.md docs/ ARCHITECTURE.md CHANGELOG.md`
Expected: prose mentions are fine; any line showing a bare `--keep-cache` in a command
(no following `true`/`false`) must be changed to `--keep-cache true`. As of this plan
only `README.md:120` is an invocation context; if the sweep surfaces others, fix them
in this commit.

- [ ] **Step 1: Add the `--storage-profile` row and update `--keep-cache` in the Tuning table**

In `README.md`, replace the `--keep-cache` table row (line 119) and insert a
`--storage-profile` row directly above the `--attr-ttl-ms` row (line 116). The
`--keep-cache` row becomes the value form:

```markdown
| `--storage-profile <ssd\|hdd\|nfs>` | unset | Preset all four tunables below for your backing medium. An explicitly-passed flag overrides the profile for that knob. See "Pick by backing store". |
```

```markdown
| `--keep-cache <true\|false>` | `false` | Keep the kernel page cache across opens. External re-tags auto-invalidate the affected files, so cached bytes never go stale. |
```

- [ ] **Step 2: Add the "Pick by backing store" subsection**

Insert immediately after the Tuning table (after line 121), using the **confirmed**
values from Task 4 (the table below shows the hypothesis values — replace any the bench
changed):

```markdown
#### Pick by backing store

`--storage-profile` sets all four tunables at once. Override any individual knob by
passing its flag explicitly (e.g. `--storage-profile nfs --max-readahead-kib 4096`).

| Profile | `max-readahead-kib` | `max-background` | `attr-ttl-ms` | `keep-cache` |
| ------- | ------------------- | ---------------- | ------------- | ------------ |
| `ssd` (= defaults) | 512 | 64 | 1000 | false |
| `hdd` | 2048 | 64 | 2000 | true |
| `nfs` | 2048 | 128 | 5000 | true |

- **ssd** — local SSD/NVMe is seek-free; the defaults are already tuned for it.
- **hdd** — larger read-ahead amortizes the ~8 ms seek over each sequential read; the
  page cache is kept so repeat opens don't re-seek.
- **nfs** — larger read-ahead and a deeper background queue hide the per-RPC network
  latency; a longer attr TTL cuts `lookup`/`getattr` round-trips (the trade-off:
  external DB edits take longer to become visible).
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document --storage-profile presets and the --keep-cache value form"
```

---

## Self-review notes (for the implementer)

- **Spec coverage:** preset flag (Task 2), docs table (Task 5), `Option`/`resolve_tunables`
  precedence incl. `--keep-cache` value form (Task 2), metrics feature passthrough +
  bench harness + wall-clock measurement + StructureOnly + concurrent driver +
  attr_ttl weak-signal handling (Tasks 1, 3, 4), confirmed-values reconciliation
  (Task 4) — all mapped.
- **Green-commit ordering:** Task 1 (manifest), Task 2 (atomic type change + all
  consumers + tests), Task 3 (shellcheck-clean script), Task 4 (bench + optional value
  edit), Task 5 (docs-only). Each is independently green for the pre-commit hook.
- **Type consistency:** `ProfileTunables` fields (`max_readahead_kib: u32`,
  `max_background: u16`, `attr_ttl_ms: u64`, `keep_cache: bool`) match the `MountArgs`
  `Option<…>` fields and `FuseConfig` (`max_readahead: u32`, `max_background: u16`,
  `ttl: Duration`, `keep_cache: bool`); `max_readahead_kib.saturating_mul(1024)` feeds
  `max_readahead` exactly as the original code did.
- **Known UX break:** bare `--keep-cache` stops parsing (now `--keep-cache true|false`).
  Pinned by `keep_cache_flag_requires_value` and documented in README + the Task 2
  commit message.
