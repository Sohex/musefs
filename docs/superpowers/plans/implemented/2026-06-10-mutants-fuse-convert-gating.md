# Gating the portable `musefs-fuse` helpers — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `musefs-fuse` mutation-testing exclusion accurate by extracting the three genuinely-gateable pure helpers into a new platform-neutral `musefs-fuse/src/convert.rs`, gating that one file in both the per-PR in-diff gate and the scheduled campaign, and rewriting the stale "thin glue, no logic" comments.

**Architecture:** This is a **behavior-preserving refactor**, not a feature. Three functions (`to_file_attr`, `assemble_dir_listing` from `lib.rs`; `cap_eff_has_sys_admin` from `platform/passthrough.rs`) plus their existing unit tests move verbatim into a new `convert.rs`. The original call sites switch to `crate::convert::…`. Then three config files (`.cargo/mutants.toml`, `scripts/mutants.sh`, `.github/workflows/mutants.yml`) narrow the exclusion so `convert.rs` is mutated while everything else stays excluded. No logic changes; the existing tests are the safety net (no new red→green TDD cycle — the tests already pass and must keep passing at the new location).

**Tech Stack:** Rust (edition 2024 workspace), `fuser`, `cargo-mutants` 27.0.0, the `check_mutant_anchors.py` guard, GitHub Actions.

**Branch:** Already on `issue-217-mutants-fuse-gating` (spec committed). Do **not** create a new branch.

**Spec:** `docs/superpowers/specs/2026-06/2026-06-10-mutants-fuse-convert-gating-design.md`

---

## File Structure

| File | Change | Responsibility after change |
| --- | --- | --- |
| `musefs-fuse/src/convert.rs` | **Create** | The three pure, mutation-tested helpers + their unit tests. Platform-neutral. |
| `musefs-fuse/src/lib.rs` | Modify | Loses `to_file_attr`/`assemble_dir_listing` + 2 tests; gains `mod convert;` and a `use`. Keeps the `Filesystem` adapter + glue. |
| `musefs-fuse/src/platform/passthrough.rs` | Modify | Loses `cap_eff_has_sys_admin` + its 4 tests; `definitely_lacks_cap_sys_admin` calls `crate::convert::cap_eff_has_sys_admin`. |
| `musefs-fuse/tests/passthrough.rs` | Modify (1 comment) | Mirror comment points at the new file path. |
| `.cargo/mutants.toml` | Modify | `exclude_globs` narrowed; header comment rewritten to the accurate rationale. |
| `scripts/mutants.sh` | Modify | New `musefs-fuse` case arm; header comment rewritten. |
| `.github/workflows/mutants.yml` | Modify | New `musefs-fuse` row in the `full` matrix. |

Two commits: **Task 1** (the refactor, must stay green — the pre-commit hook runs the full workspace test suite) and **Task 2** (the config + comments). Tasks 2's three files are coupled and land together.

---

## Task 1: Extract the three pure helpers into `convert.rs`

**Files:**
- Create: `musefs-fuse/src/convert.rs`
- Modify: `musefs-fuse/src/lib.rs` (remove fns at 108–166; remove 2 tests at 597–631 and 706–721; add `mod convert;` near line 26; add a `use`; fix test-module imports)
- Modify: `musefs-fuse/src/platform/passthrough.rs` (remove fn at 99–108; remove tests at 110–139; edit caller at line 95)
- Modify: `musefs-fuse/tests/passthrough.rs` (comment at line 124)

- [ ] **Step 1: Create `musefs-fuse/src/convert.rs` with the three helpers and their tests**

Create the file with exactly this content (the three function bodies are copied verbatim from their current locations; only visibility and the `cfg` gate on the cap parser change):

```rust
//! Pure, platform-neutral conversions between `musefs-core` types and the FUSE
//! layer's `fuser` types, plus the `/proc/self/status` capability parser.
//!
//! These helpers carry the only mutation-tested logic in `musefs-fuse`: they
//! are the one file in this crate left in scope by `.cargo/mutants.toml`. The
//! `Filesystem` trait adapter and session glue in `lib.rs`, and the
//! `cfg(macos)` platform code, are excluded (glue / uncoverable on the Linux
//! mutation runner). See the spec at
//! `docs/superpowers/specs/2026-06/2026-06-10-mutants-fuse-convert-gating-design.md`.

use std::time::{Duration, SystemTime};

use fuser::{FileAttr, FileType, INodeNo};
use musefs_core::Attr;

/// Translate a core `Attr` into a `fuser::FileAttr`. Read-only perms (`0o555`
/// dirs, `0o444` files). A zero `mtime_secs` (e.g. synthetic directories) falls
/// back to `fallback_mtime` so tools don't see a 1970 timestamp.
pub(crate) fn to_file_attr(
    attr: &Attr,
    uid: u32,
    gid: u32,
    fallback_mtime: SystemTime,
) -> FileAttr {
    let mtime = if attr.mtime_secs > 0 {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(
                u64::try_from(attr.mtime_secs).expect("guarded by mtime_secs > 0"),
            )
    } else {
        fallback_mtime
    };
    let (kind, perm, nlink) = if attr.is_dir {
        (FileType::Directory, 0o555, 2)
    } else {
        (FileType::RegularFile, 0o444, 1)
    };
    FileAttr {
        ino: INodeNo(attr.inode),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Assemble a directory's readdir listing: `.`, `..`, the children, then the
/// optional Spotlight marker. Pure (no DB/tree access) so it is unit-testable.
pub(crate) fn assemble_dir_listing(
    ino: u64,
    parent: u64,
    entries: Vec<(String, u64, bool)>,
    marker: Option<(u64, FileType, String)>,
) -> Vec<(u64, FileType, String)> {
    let mut listing: Vec<(u64, FileType, String)> = Vec::with_capacity(entries.len() + 2);
    listing.push((ino, FileType::Directory, ".".to_string()));
    listing.push((parent, FileType::Directory, "..".to_string()));
    for (name, child, is_dir) in entries {
        let kind = if is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        listing.push((child, kind, name));
    }
    if let Some(entry) = marker {
        listing.push(entry);
    }
    listing
}

/// Parse the `CapEff:` line of `/proc/self/status`; `None` when absent or
/// malformed. Pure string parsing, so it lives here (OS-neutral) rather than in
/// the Linux-only passthrough module.
///
/// Gated `cfg(any(target_os = "linux", test))`: its only non-test caller,
/// `platform::passthrough`'s `definitely_lacks_cap_sys_admin`, is Linux-only, so
/// a `pub(crate)` fn left compiled-but-unused on a non-Linux **non-test** build
/// would trip the `-D warnings` dead_code gate (the macOS clippy job is the only
/// non-Linux gate; FreeBSD is cross-linted). This gate compiles it exactly where
/// it is used — the Linux lib build and every platform's test build.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn cap_eff_has_sys_admin(status: &str) -> Option<bool> {
    const CAP_SYS_ADMIN_BIT: u32 = 21;
    let hex = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))?
        .trim();
    let mask = u64::from_str_radix(hex, 16).ok()?;
    Some(mask & (1 << CAP_SYS_ADMIN_BIT) != 0)
}

#[cfg(test)]
mod tests {
    use super::{assemble_dir_listing, cap_eff_has_sys_admin, to_file_attr};
    use fuser::{FileType, INodeNo};
    use musefs_core::Attr;
    use std::time::{Duration, SystemTime};

    #[test]
    fn converts_dir_and_file_attrs() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

        let dir = Attr {
            inode: 1,
            is_dir: true,
            size: 0,
            mtime_secs: 0,
        };
        let fa = to_file_attr(&dir, 501, 20, fallback);
        assert_eq!(fa.ino, INodeNo(1));
        assert_eq!(fa.kind, FileType::Directory);
        assert_eq!(fa.perm, 0o555);
        assert_eq!(fa.uid, 501);
        assert_eq!(fa.gid, 20);
        // mtime_secs == 0 falls back to the supplied mount time.
        assert_eq!(fa.mtime, fallback);

        let file = Attr {
            inode: 9,
            is_dir: false,
            size: 4096,
            mtime_secs: 1_700_000_000,
        };
        let fa = to_file_attr(&file, 501, 20, fallback);
        assert_eq!(fa.kind, FileType::RegularFile);
        assert_eq!(fa.perm, 0o444);
        assert_eq!(fa.size, 4096);
        assert_eq!(fa.blocks, 8); // 4096 / 512
        assert_eq!(
            fa.mtime,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
    }

    #[test]
    fn assemble_dir_listing_puts_dot_and_dotdot_first() {
        let entries = vec![
            ("Song.flac".to_string(), 42, false),
            ("Sub".to_string(), 43, true),
        ];
        let listing = assemble_dir_listing(7, 3, entries, None);
        assert_eq!(listing.len(), 4);
        assert_eq!(listing[0], (7, FileType::Directory, ".".to_string()));
        assert_eq!(listing[1], (3, FileType::Directory, "..".to_string()));
        assert_eq!(
            listing[2],
            (42, FileType::RegularFile, "Song.flac".to_string())
        );
        assert_eq!(listing[3], (43, FileType::Directory, "Sub".to_string()));
    }

    #[test]
    fn cap_eff_parser_root_mask_has_sys_admin() {
        assert_eq!(
            cap_eff_has_sys_admin("CapPrm:\t0000003fffffffff\nCapEff:\t0000003fffffffff\n"),
            Some(true)
        );
    }

    #[test]
    fn cap_eff_parser_zero_mask_lacks_sys_admin() {
        assert_eq!(
            cap_eff_has_sys_admin("CapEff:\t0000000000000000\n"),
            Some(false)
        );
    }

    #[test]
    fn cap_eff_parser_missing_line_returns_none() {
        assert_eq!(cap_eff_has_sys_admin("Name:\tfoo\nUid:\t1000\n"), None);
    }

    #[test]
    fn cap_eff_parser_garbage_hex_returns_none() {
        assert_eq!(cap_eff_has_sys_admin("CapEff:\tnothex\n"), None);
    }
}
```

- [ ] **Step 2: Register the module and import the two helpers in `lib.rs`**

In `musefs-fuse/src/lib.rs`, find the module declaration `mod platform;` (around line 26) and add `mod convert;` directly above it:

```rust
mod convert;
mod platform;
```

Then add an import so the existing unqualified call sites keep working. Place it with the other `use crate::`/`use musefs_core::` lines near the top of the file (immediately after the `use musefs_core::convert::usize_from;` line is a natural home):

```rust
use crate::convert::{assemble_dir_listing, to_file_attr};
```

- [ ] **Step 3: Delete the two moved functions from `lib.rs`**

Delete the entire `to_file_attr` function (its doc comment + body, currently lines 108–142) and the entire `assemble_dir_listing` function (doc comment + body, currently lines 143–166). These are the two functions you copied into `convert.rs` in Step 1. The call sites at lines 175 (`build_dir_listing`), 326, and 340 are unchanged — they now resolve through the `use` added in Step 2.

Then delete the file-level import that those functions were the only user of: `to_file_attr` was the sole reference to `Attr`, so remove line 19:

```rust
use musefs_core::Attr;
```

Do **not** touch the neighbouring `use musefs_core::CoreError;` (line 20, used throughout `errno`/`reply_errno`), nor `INodeNo`/`Duration`/`SystemTime` in the top `use` block (still used by the `Filesystem` impl and `FuseConfig`/`mount_time`).

- [ ] **Step 4: Delete the two moved tests from `lib.rs` and fix the test-module imports**

In `lib.rs`'s `#[cfg(test)] mod tests` (starts ~line 573): delete the `converts_dir_and_file_attrs` test (currently 597–631) and the `assemble_dir_listing_puts_dot_and_dotdot_first` test (currently 706–721). Both moved to `convert.rs`.

Then update that module's import header. It currently reads **five** lines (verify verbatim against the file before editing — do not trust this paraphrase):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fuser::FileType;
    use musefs_core::Attr;
    use musefs_core::CoreError;
    use std::time::{Duration, SystemTime};
```

Change it to the following — removing `fuser::FileType`, `musefs_core::Attr`, and `SystemTime` (only the two deleted tests used those), and **keeping `CoreError`** (the staying `maps_core_errors_to_errno` test uses it) and `Duration` (the staying `fuse_config_default_is_conservative` test uses it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use musefs_core::CoreError;
    use std::time::Duration;
```

Leave the separate `mod errno_tests` (after the `tests` module) untouched.

- [ ] **Step 5: Move the cap parser out of `passthrough.rs` and repoint its caller**

In `musefs-fuse/src/platform/passthrough.rs`:

(a) Delete the `cap_eff_has_sys_admin` function (doc comment + body, currently lines 99–108) — moved to `convert.rs`.

(b) Delete the `#[cfg(test)] mod tests { … }` block inside `mod imp` (currently lines 110–139) — its four `cap_eff_parser_*` tests moved to `convert.rs`.

(c) Repoint the caller. In `definitely_lacks_cap_sys_admin` (line 95) change:

```rust
            .and_then(|s| cap_eff_has_sys_admin(&s))
```

to:

```rust
            .and_then(|s| crate::convert::cap_eff_has_sys_admin(&s))
```

`definitely_lacks_cap_sys_admin` itself, `reply_open`, `request_capabilities`, `PassthroughState`, and the `#[cfg(not(target_os = "linux"))]` stub block all stay exactly as they are.

- [ ] **Step 6: Fix the stale mirror comment in the e2e test**

In `musefs-fuse/tests/passthrough.rs`, the `have_cap_sys_admin` helper (line ~124) has a comment pointing at the wrong file (it predates the parser even being in `passthrough.rs`). Change:

```rust
/// Mirrors the daemon's private `cap_eff_has_sys_admin` (src/lib.rs) — keep
/// the two predicates in sync.
```

to:

```rust
/// Mirrors the daemon's `cap_eff_has_sys_admin` (src/convert.rs) — keep the two
/// predicates in sync.
```

(The mirror stays: integration tests live in a separate crate and can't see the `pub(crate)` parser.)

- [ ] **Step 7: Build the crate**

Run: `cargo build -p musefs-fuse`
Expected: clean build, no errors.

- [ ] **Step 8: Run the fuse crate's tests**

Run: `cargo test -p musefs-fuse`
Expected: PASS. The six relocated tests now run under `musefs-fuse` (the four `cap_eff_parser_*` tests now run on every platform, not just Linux). Confirm `converts_dir_and_file_attrs`, `assemble_dir_listing_puts_dot_and_dotdot_first`, and the four `cap_eff_parser_*` tests appear in the output and pass.

- [ ] **Step 9: Lint (Linux) — this catches any leftover unused import**

Run: `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: clean. This is the backstop for the import trimming in Steps 3–4: if you missed deleting the file-level `use musefs_core::Attr;` (line 19) or over-trimmed the test-module header, clippy fires `unused_imports` / `unresolved import` here. Remove exactly the flagged unused import (or restore `CoreError` if you cut it), then re-run until clean.

- [ ] **Step 10: Cross-lint a non-Linux target — this is the dead-code check for the moved cap parser**

Ensure the target is installed, then cross-lint:

```bash
rustup target add x86_64-unknown-freebsd
cargo clippy -p musefs-fuse --target x86_64-unknown-freebsd --all-targets -- -D warnings
```

Expected: clean — specifically **no `dead_code` on `cap_eff_has_sys_admin`**. If it fires, the `#[cfg(any(target_os = "linux", test))]` gate from Step 1 is missing or wrong; fix it before proceeding. (This is the failure mode the spec's Risk #2 calls out.)

- [ ] **Step 11: Format**

Run: `cargo fmt --all` then `cargo fmt --all --check`
Expected: the `--check` exits 0 (no diff).

- [ ] **Step 12: Commit**

```bash
git add musefs-fuse/src/convert.rs musefs-fuse/src/lib.rs \
        musefs-fuse/src/platform/passthrough.rs musefs-fuse/tests/passthrough.rs
git commit -m "refactor(fuse): extract pure convert helpers into convert.rs (#217)"
```

The pre-commit hook runs fmt + clippy `-D warnings` + the full workspace test suite + ruff; it must pass. (It does **not** run the FreeBSD cross-lint — that is why Step 10 is a manual gate here and a CI job later.)

---

## Task 2: Gate `convert.rs` in the mutation config and rewrite the stale comments

**Files:**
- Modify: `.cargo/mutants.toml` (`exclude_globs` + leading comment)
- Modify: `scripts/mutants.sh` (new case arm + header comment)
- Modify: `.github/workflows/mutants.yml` (matrix row)

These three ship together (the matrix row invokes the new case arm; landing only one yields a red scheduled job).

- [ ] **Step 1: Narrow `exclude_globs` and rewrite the leading comment in `.cargo/mutants.toml`**

Replace the file's leading comment block + `exclude_globs` array (currently lines 1–29, ending at the `]` of `exclude_globs`) with:

```toml
# cargo-mutants workspace config.
#
# Crates / files out of scope for mutation testing, with the reason each is
# excluded (the in-scope crates mutate a hand-picked --file allowlist in
# scripts/mutants.sh; this list governs both that campaign and the per-PR
# --in-diff gate). See
# docs/superpowers/specs/2026-06/2026-06-10-mutants-fuse-convert-gating-design.md.
#
# musefs-fuse: only src/convert.rs is in scope — the portable, mutation-tested
# pure helpers (to_file_attr, assemble_dir_listing, cap_eff_has_sys_admin),
# killable by plain unit tests on the Linux runner. The rest is excluded:
#   - src/lib.rs: the Filesystem trait adapter + session/mount glue (its mutants
#     all survive plain `cargo test`), plus helpers whose only mutants are
#     unviable (errno, open_flags), equivalent (reply_errno selects only a log
#     level), or core-I/O (build_dir_listing takes &Musefs).
#   - src/platform/**: the logic-bearing parts are #[cfg(target_os = "macos")]
#     and uncoverable on the Linux mutation runner — the macOS code is cfg'd out
#     there so its mutants always survive, and its tests only compile on macOS.
#     Gating it needs a macOS mutation leg (deferred). NB: narrowing the glob to
#     specific files means any future top-level musefs-fuse/src/*.rs becomes
#     in-scope automatically — intended, but worth knowing.
#
# musefs-cli: parse_mount_config / From<CliMode> produce only unviable mutants;
# unmount_commands is the lone caught helper and is not worth un-excluding
# signal.rs's thread/exit() glue for. The `musefs` binary is a one-line
# entrypoint. Both fully excluded.
#
# musefs-latencyfs is NOT excluded here: it carries real logic (the inode map,
# latency table, attr mapping, passthrough op behavior), so it gets its own
# mutation leg in scripts/mutants.sh / mutants.yml that installs libfuse and runs
# the #[ignore]d mounted tests (`-- -- --include-ignored`). The fast per-PR
# `--in-diff` gate excludes it separately (it is mountless and can't kill its
# mutants); see .github/workflows/mutants.yml.
#
# musefs-core/src/metrics.rs is feature-gated instrumentation: its active impl is
# `#[cfg(feature = "metrics")]` and is only compiled (with its tests) under
# `--features metrics`. The per-PR `--in-diff` gate runs cargo-mutants without that
# feature, so the counter bodies aren't compiled and any mutation there is
# unobservable — reported MISSED yet unkillable in that config. The counters are a
# pure observability shim with no serving/correctness role and are covered
# functionally by the feature-gated tests in that file, so exclude them from
# mutation rather than carry permanently-surviving mutants.
exclude_globs = [
    "musefs-fuse/src/lib.rs",
    "musefs-fuse/src/platform/**",
    "musefs-cli/**",
    "musefs/**",
    "musefs-core/src/metrics.rs",
]
```

Leave the entire `exclude_re = [ … ]` array and its comments (everything after this block) untouched.

- [ ] **Step 2: Add the `musefs-fuse` case arm and rewrite the header in `scripts/mutants.sh`**

(a) In the header comment, the lines that currently read:

```sh
# musefs-cli, musefs-fuse, and the `musefs` binary are thin glue with no real
# logic and are excluded from mutation entirely via .cargo/mutants.toml.
```

Replace with:

```sh
# musefs-fuse is mostly thin glue, but its src/convert.rs holds portable
# pure helpers (to_file_attr, assemble_dir_listing, cap_eff_has_sys_admin) that
# ARE mutation-tested: the `musefs-fuse` leg below mutates that one file with a
# plain mountless `cargo test`. The Filesystem adapter, session glue, and
# cfg(macos) platform code stay excluded via .cargo/mutants.toml, as do
# musefs-cli and the `musefs` binary (thin glue with no gateable logic).
```

(b) In the `case "$crate" in` block, add a `musefs-fuse)` arm. Place it after the `musefs-format)` arm and before `musefs-latencyfs)`:

```sh
    musefs-fuse)
      # Mountless: convert.rs's helpers are exercised by plain (non-#[ignore]d)
      # unit tests, so this leg needs no /dev/fuse or libfuse mount (unlike the
      # musefs-latencyfs leg). Scoped to the one in-scope file.
      run_crate musefs-fuse --test-workspace=false \
        --file musefs-fuse/src/convert.rs
      ;;
```

Do not add `musefs-fuse` to the default crate list (`crates=(musefs-db musefs-core musefs-format)`); it runs only when named explicitly or from the CI matrix.

- [ ] **Step 3: Add the `musefs-fuse` row to the `full` matrix in `.github/workflows/mutants.yml`**

In the `full` job's `strategy.matrix.include` list, add a row for `musefs-fuse`. Place it after the four `musefs-format-*` shard rows and before the `musefs-latencyfs` row:

```yaml
          - { crate: musefs-fuse, label: musefs-fuse }
```

It intentionally has **no** `shard` key (whole-crate; `convert.rs` is tiny). The existing `Install FUSE` step is gated `if: matrix.crate == 'musefs-latencyfs'`, so the fuse leg correctly skips it — `convert.rs`'s tests never mount. No other edits to this file (the `in-diff` job needs none: `convert.rs` is auto-in-scope once out of `exclude_globs`, and `lib.rs`/`platform/**` stay excluded via the narrowed globs).

- [ ] **Step 4: Validate the exclusion anchors still pass**

The glob narrowing must not orphan any `exclude_re` entry (the guard fails if an `exclude_re` matches a file that is no longer in `exclude_globs`, or if any entry's site count drifts):

```bash
cargo mutants --no-config --list --json > /tmp/mutants-list.json
python3 scripts/check_mutant_anchors.py --mutants-json /tmp/mutants-list.json
```

Expected: `OK: N exclude_re entries validated against M mutants.` (no failures). The only `convert\.rs` anchor is `musefs-format/src/convert\.rs` (crate-prefixed), so it cannot collide with the new `musefs-fuse/src/convert.rs`.

- [ ] **Step 5: Prove `convert.rs` is survivor-free under mutation (the gate actually has teeth)**

Run cargo-mutants scoped to the new file (copy-mode under `/tmp` per the local-mutants convention):

```bash
TMPDIR=/tmp cargo mutants -p musefs-fuse --file musefs-fuse/src/convert.rs -j2
```

Expected: every mutant **caught**, `0 missed`, `0 timeout`. This re-verifies at the new path what the investigation saw at the old one (`to_file_attr`'s `>`/`+` mutants, `assemble_dir_listing -> vec![]`, and the `cap_eff_has_sys_admin` bit/compare mutants are all killed by the relocated tests). If anything is MISSED, stop — the move dropped a test or an import; do not exclude it to go green.

- [ ] **Step 6: Commit**

```bash
git add .cargo/mutants.toml scripts/mutants.sh .github/workflows/mutants.yml
git commit -m "build(mutants): gate musefs-fuse/convert.rs; correct stale exclusions (#217)"
```

The pre-commit hook runs (fmt/clippy/tests/ruff); config-only changes keep it green.

---

## Final verification

- [ ] **Whole-workspace green:** `cargo test --workspace` — PASS.
- [ ] **Lint clean both ways:** `cargo clippy --all-targets -- -D warnings` and `cargo clippy --target x86_64-unknown-freebsd --all-targets -- -D warnings` — both clean.
- [ ] **Format:** `cargo fmt --all --check` — exit 0.
- [ ] **Fuzz unaffected:** no format-layer API changed, so no `cargo +nightly fuzz build` needed (sanity: this PR touches only `musefs-fuse` + mutation config).
- [ ] **Two commits present** on `issue-217-mutants-fuse-gating`: the refactor, then the config. `git log --oneline -3` shows both atop the spec commit.

## Spec-coverage check (self-review)

- Spec §1 (new `convert.rs`, the three helpers, `pub(crate)` + cap `cfg` gate, test split, caller repoint) → Task 1 Steps 1–6.
- Spec §1 "survivor-free, re-verify at new path" → Task 2 Step 5.
- Spec §2 (`exclude_globs` narrowing + rewritten comment, no new `exclude_re`, glob-fragility note) → Task 2 Step 1; anchor guard → Step 4.
- Spec §3 (`mutants.sh` case arm + header `:5-6` rewrite, unsharded) → Task 2 Step 2.
- Spec §4 (`mutants.yml` matrix row, in-diff no change, §3+§4 coupled) → Task 2 Step 3 (+ shipped with Step 2 in one commit).
- Spec "Risks": dead-code → Task 1 Steps 1 & 10; libfuse build check → Task 1 Step 7 / `full` leg has no FUSE-install gate (Task 2 Step 3); macOS regression → Task 1 Step 10.
- Spec "Out of scope" (`build_dir_listing`, macOS Spotlight, `musefs-cli`) → deliberately untouched; documented in the rewritten comments (Task 2 Step 1).
