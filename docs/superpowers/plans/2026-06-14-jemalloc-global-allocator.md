# jemalloc Global Allocator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `musefs` binary use jemalloc as its global allocator (with a background purge thread) to bound RSS under long-lived, high-churn FUSE load, behind a default-on `jemalloc` feature that packagers/debuggers can disable.

**Architecture:** The allocator override lives only in the binary crate (`musefs/src/main.rs`); library crates, tests, benches, and the sanitizer CI jobs are untouched. jemalloc comes from `tikv-jemallocator`; the background purge thread is turned on at startup via the safe `tikv-jemalloc-ctl` API (no `unsafe`, so the workspace `unsafe_code = "deny"` lint is satisfied). A committed Linux churn benchmark measures steady-state RSS to gate the change; docs and the release pipeline are updated for the new dependency.

**Tech Stack:** Rust 2024, `tikv-jemallocator` + `tikv-jemalloc-ctl` 0.7, `cargo-zigbuild` (release cross-builds), Bash + `/proc` (RSS bench).

**Source spec:** `docs/superpowers/specs/2026-06-14-jemalloc-global-allocator-design.md` (issue #360 part A; telemetry part B is a separate spec, out of scope).

**Pre-commit note:** every non-docs commit runs `cargo fmt`, `cargo clippy --all-targets -D warnings`, the **full workspace test suite**, and `shellcheck` over tracked shell files. Each code/script commit below must be green. Docs-only commits skip the cargo legs. No `.cargo/mutants.toml` re-anchoring is needed — its anchors live in `musefs-core`/`musefs-format`, which this plan never edits.

---

## File Structure

- `musefs/Cargo.toml` — **modify**: add the `jemalloc` feature (default-on) and the three optional deps.
- `musefs/src/main.rs` — **modify**: add `#[global_allocator]`, the background-thread helper, the startup call, and unit tests proving jemalloc is wired and the purge thread is enabled.
- `scripts/rss-churn-bench.sh` — **create**: Linux steady-state RSS churn benchmark comparing system-malloc vs jemalloc builds.
- `BENCHMARKS.md` — **modify**: new allocator/RSS section recording methodology, parameters, numbers, and the ship decision.
- `README.md` — **modify**: one line under "Building from source" on the default allocator + `--no-default-features` opt-out.
- `CONTRIBUTING.md` — **modify**: note the `jemalloc` feature and the release smoke-build step.
- `.github/workflows/release.yml` — **modify only if** a release target fails to cross-build `jemalloc-sys` (per-target `--no-default-features` fallback). Conditional; see Task 6.

---

## Task 1: Wire jemalloc as the global allocator

**Files:**
- Modify: `musefs/Cargo.toml`
- Modify/Test: `musefs/src/main.rs`

- [ ] **Step 1: Add the feature + optional deps to `musefs/Cargo.toml`**

Insert a `[features]` block immediately after the `[[bin]]` block (before `[dependencies]`), and add the two optional deps to `[dependencies]`:

```toml
[features]
default = ["jemalloc"]
jemalloc = ["dep:tikv-jemallocator", "dep:tikv-jemalloc-ctl"]

[dependencies]
clap = { version = "4", features = ["derive"] }
env_logger = "0.11"
musefs-cli = { path = "../musefs-cli", version = "1.0.0" }
tikv-jemallocator = { version = "0.7", optional = true }
tikv-jemalloc-ctl = { version = "0.7", optional = true }
```

(jemalloc's stats and `background_threads_runtime_support` are on by default via `tikv-jemalloc-sys`; no extra cargo feature is required.)

- [ ] **Step 2: Write the failing test in `musefs/src/main.rs`**

Append this module at the end of `musefs/src/main.rs`. The test allocates a 4 MiB buffer and asks jemalloc how much it has allocated; jemalloc only reports a large figure if it is actually serving the global allocator.

```rust
#[cfg(all(test, feature = "jemalloc"))]
mod tests {
    #[test]
    fn jemalloc_is_the_global_allocator() {
        // mallctl reads talk to the linked jemalloc runtime; `allocated` only
        // climbs past a megabyte if jemalloc is serving #[global_allocator].
        let buf: Vec<u8> = vec![0u8; 4 * 1024 * 1024];
        std::hint::black_box(&buf);
        tikv_jemalloc_ctl::epoch::advance().unwrap();
        let allocated = tikv_jemalloc_ctl::stats::allocated::read().unwrap();
        assert!(
            allocated >= 1 << 20,
            "jemalloc reports {allocated} bytes allocated; not wired as #[global_allocator]"
        );
        drop(buf);
    }
}
```

- [ ] **Step 3: Run the test and confirm it FAILS**

Run: `cargo test -p musefs jemalloc_is_the_global_allocator`
Expected: FAIL — `allocated` is ~0 because the system allocator is still serving the `Vec`, jemalloc is linked but unused. (The assertion message prints the tiny byte count.)

- [ ] **Step 4: Add the global allocator to `musefs/src/main.rs`**

Insert these lines between the `use` line and `fn main()`:

```rust
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
```

- [ ] **Step 5: Run the test and confirm it PASSES**

Run: `cargo test -p musefs jemalloc_is_the_global_allocator`
Expected: PASS — the 4 MiB `Vec` now flows through jemalloc, so `allocated >= 1 MiB`.

- [ ] **Step 6: Confirm the opt-out still builds**

Run: `cargo build -p musefs --no-default-features`
Expected: builds cleanly (system malloc; the `#[cfg(feature = "jemalloc")]` items and the test module compile out).

- [ ] **Step 7: Lint + format**

Run: `cargo clippy -p musefs --all-targets -- -D warnings && cargo fmt`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add musefs/Cargo.toml musefs/src/main.rs Cargo.lock
git commit -m "feat(musefs): jemalloc global allocator behind default-on feature (#360)"
```

(The pre-commit hook runs the full workspace test suite under default features, exercising the new test.)

---

## Task 2: Enable the background purge thread at startup

**Files:**
- Modify: `musefs/Cargo.toml`
- Modify/Test: `musefs/src/main.rs`

- [ ] **Step 1: Add the `log` optional dep to the `jemalloc` feature**

In `musefs/Cargo.toml`, extend the feature and add the dep:

```toml
jemalloc = ["dep:tikv-jemallocator", "dep:tikv-jemalloc-ctl", "dep:log"]
```

```toml
log = { version = "0.4", optional = true }
```

(Add the `log` line to `[dependencies]` alongside the other optional deps.)

- [ ] **Step 2: Write the failing test in `musefs/src/main.rs`**

Add these two tests inside the existing `#[cfg(all(test, feature = "jemalloc"))] mod tests` block (next to `jemalloc_is_the_global_allocator`):

```rust
    #[cfg(target_os = "linux")]
    #[test]
    fn background_thread_enables_on_linux() {
        super::enable_jemalloc_background_thread();
        assert!(
            tikv_jemalloc_ctl::background_thread::read().unwrap(),
            "background_thread should be on after enable() on linux"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn enable_background_thread_does_not_panic_off_linux() {
        // jemalloc lacks background-thread support on some platforms (macOS);
        // the helper must swallow the error rather than panic.
        super::enable_jemalloc_background_thread();
    }
```

- [ ] **Step 3: Run the test and confirm it FAILS**

Run: `cargo test -p musefs background_thread`
Expected: FAIL to compile — `enable_jemalloc_background_thread` does not exist yet (`cannot find function ... in module super`).

- [ ] **Step 4: Add the helper and call it from `main`**

In `musefs/src/main.rs`, add the helper just below the `GLOBAL` static:

```rust
/// Enable jemalloc's background purge thread so an idle daemon returns dirty
/// pages to the OS (the RSS-creep fix in #360). Best-effort: unsupported on some
/// platforms (notably macOS), where it logs at debug and continues — jemalloc
/// stays active and still purges on allocation activity.
#[cfg(feature = "jemalloc")]
fn enable_jemalloc_background_thread() {
    if let Err(e) = tikv_jemalloc_ctl::background_thread::write(true) {
        log::debug!("jemalloc background_thread unavailable: {e}");
    }
}
```

Then call it in `main`, right after `env_logger…init()` and before `run(...)`:

```rust
    #[cfg(feature = "jemalloc")]
    enable_jemalloc_background_thread();
```

- [ ] **Step 5: Run the test and confirm it PASSES**

Run: `cargo test -p musefs background_thread`
Expected: PASS on Linux (`background_thread_enables_on_linux` asserts `true`).

- [ ] **Step 6: Confirm the opt-out still builds**

Run: `cargo build -p musefs --no-default-features`
Expected: builds cleanly (helper + call compile out).

- [ ] **Step 7: Lint + format**

Run: `cargo clippy -p musefs --all-targets -- -D warnings && cargo fmt`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add musefs/Cargo.toml musefs/src/main.rs Cargo.lock
git commit -m "feat(musefs): enable jemalloc background purge thread at startup (#360)"
```

---

## Task 3: RSS churn benchmark script

**Files:**
- Create: `scripts/rss-churn-bench.sh`

- [ ] **Step 1: Create `scripts/rss-churn-bench.sh` with this exact content**

```bash
#!/usr/bin/env bash
# Steady-state RSS churn benchmark for the musefs daemon (issue #360 part A).
# Drives concurrent open/read/release churn against a mounted store and reports
# the MEDIAN VmRSS over the flattened tail (steady state, NOT peak RSS), for the
# system-malloc and jemalloc builds, then prints a ship/investigate decision.
#
# Linux-only: VmRSS comes from /proc/<pid>/status.
#
# Env knobs (defaults in parens):
#   DB           musefs store path                         (required)
#   MOUNT        mountpoint under $HOME                     ($HOME/.musefs-rss-mnt)
#   WORKERS      concurrent reader threads                 (nproc)
#   FILES        distinct files to churn                   (500)
#   CYCLES       1-second RSS samples per variant          (200)
#   WARMUP       leading samples discarded                 (20)
#   REFRESH_CMD  shell command run every REFRESH_SECS      (none)
#   REFRESH_SECS refresh cadence in seconds                (30)
#   BINARIES     override builds: "sysmalloc=/p jemalloc=/p" (built from repo)
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "rss-churn-bench: Linux-only (needs /proc/<pid>/status VmRSS)" >&2
  exit 1
fi

: "${DB:?set DB to a musefs store path}"
MOUNT="${MOUNT:-$HOME/.musefs-rss-mnt}"
WORKERS="${WORKERS:-$(nproc)}"
FILES="${FILES:-500}"
CYCLES="${CYCLES:-200}"
WARMUP="${WARMUP:-20}"
REFRESH_SECS="${REFRESH_SECS:-30}"

build_variants() {
  echo "building system-malloc and jemalloc release binaries..." >&2
  cargo build --release -p musefs --no-default-features >&2
  cp target/release/musefs /tmp/musefs-sysmalloc
  cargo build --release -p musefs >&2
  cp target/release/musefs /tmp/musefs-jemalloc
  echo "sysmalloc=/tmp/musefs-sysmalloc jemalloc=/tmp/musefs-jemalloc"
}

# stdin: one integer per line -> median of the last 25% of lines.
median_tail() {
  local n tail_start
  mapfile -t vals
  n="${#vals[@]}"
  tail_start=$(( n - n / 4 ))
  printf '%s\n' "${vals[@]:tail_start}" | sort -n | awk '
    { a[NR] = $1 }
    END { if (NR % 2) print a[(NR + 1) / 2]; else print (a[NR / 2] + a[NR / 2 + 1]) / 2 }'
}

run_variant() {
  local bin="$1"
  mkdir -p "$MOUNT"
  "$bin" mount "$DB" "$MOUNT" &
  local mpid=$!
  local i
  for i in $(seq 1 50); do
    mountpoint -q "$MOUNT" && break
    sleep 0.1
  done
  local targets=()
  mapfile -t targets < <(find "$MOUNT" -type f | head -n "$FILES")
  if [ "${#targets[@]}" -eq 0 ]; then
    echo "no files found under $MOUNT" >&2
    fusermount3 -u "$MOUNT" 2>/dev/null || true
    return 1
  fi
  local stop
  stop="$(mktemp)"
  rm -f "$stop"
  local pids=()
  for i in $(seq 1 "$WORKERS"); do
    (
      while [ ! -e "$stop" ]; do
        for f in "${targets[@]}"; do
          [ -e "$stop" ] && break
          cat "$f" >/dev/null 2>&1 || true
        done
      done
    ) &
    pids+=("$!")
  done
  local rpid=""
  if [ -n "${REFRESH_CMD:-}" ]; then
    (
      while [ ! -e "$stop" ]; do
        sleep "$REFRESH_SECS"
        # shellcheck disable=SC2086
        eval $REFRESH_CMD >/dev/null 2>&1 || true
      done
    ) &
    rpid=$!
  fi
  local samples=()
  for i in $(seq 1 "$CYCLES"); do
    sleep 1
    samples+=("$(awk '/^VmRSS:/ { print $2 }' "/proc/$mpid/status" 2>/dev/null || echo 0)")
  done
  : > "$stop"
  wait "${pids[@]}" 2>/dev/null || true
  [ -n "$rpid" ] && { wait "$rpid" 2>/dev/null || true; }
  rm -f "$stop"
  fusermount3 -u "$MOUNT" 2>/dev/null || true
  wait "$mpid" 2>/dev/null || true
  printf '%s\n' "${samples[@]:$WARMUP}" | median_tail
}

main() {
  local spec="${BINARIES:-$(build_variants)}"
  echo "label,steady_state_rss_kib"
  local sys_rss="" jem_rss="" pair label bin rss
  # shellcheck disable=SC2086
  for pair in $spec; do
    label="${pair%%=*}"
    bin="${pair#*=}"
    rss="$(run_variant "$bin")"
    echo "$label,$rss"
    case "$label" in
      *sys*) sys_rss="$rss" ;;
      *jem*) jem_rss="$rss" ;;
    esac
  done
  if [ -n "$sys_rss" ] && [ -n "$jem_rss" ]; then
    if [ "$jem_rss" -le "$sys_rss" ]; then
      echo "decision: SHIP jemalloc (steady-state ${jem_rss} kiB <= sysmalloc ${sys_rss} kiB)"
    else
      echo "decision: INVESTIGATE — jemalloc ${jem_rss} kiB > sysmalloc ${sys_rss} kiB"
    fi
  fi
}

main "$@"
```

- [ ] **Step 2: Make it executable and pass shellcheck**

Run: `chmod +x scripts/rss-churn-bench.sh && shellcheck scripts/rss-churn-bench.sh`
Expected: no findings (the two `eval`/word-split spots carry `# shellcheck disable` comments).

- [ ] **Step 3: Smoke-run the script against the live-mount harness**

Prep the harness DB once (the real store; copy to tmpfs so the bench isn't disk-bound):

Run:
```bash
cp ~/musefs.db /tmp/musefs.db
DB=/tmp/musefs.db MOUNT="$HOME/.musefs-rss-mnt" \
  WORKERS=2 FILES=5 CYCLES=5 WARMUP=1 \
  scripts/rss-churn-bench.sh
```
Expected: it builds both binaries, then prints a `label,steady_state_rss_kib` header, two data rows (`sysmalloc,<n>` and `jemalloc,<n>`), and a `decision:` line. (Numbers are tiny/noisy at these smoke parameters — this step only proves the harness runs end-to-end.)

- [ ] **Step 4: Commit**

```bash
git add scripts/rss-churn-bench.sh
git commit -m "test(scripts): steady-state RSS churn benchmark for the allocator (#360)"
```

(The pre-commit `shellcheck` leg lints the new script.)

---

## Task 4: README + CONTRIBUTING documentation

**Files:**
- Modify: `README.md`
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Add the allocator note to `README.md`**

In `README.md`, under `### Building from source`, after the line
`The same `fuse3` runtime requirement as the prebuilt binaries applies.`
add a new paragraph:

```markdown
The binary uses **jemalloc** as its global allocator by default (it bounds
resident memory for the long-lived mount daemon under heavy concurrent reads).
Distribution packagers or anyone debugging memory with valgrind/heaptrack can
build against the system allocator instead with
`cargo build -p musefs --no-default-features` (or `cargo install musefs
--no-default-features`).
```

- [ ] **Step 2: Add the feature note to `CONTRIBUTING.md` Build & test**

In `CONTRIBUTING.md`, under `## Build & test`, after the closing ``` of the
first code block (the `cargo build … cargo fmt` block), add:

```markdown
The `musefs` binary enables the default-on `jemalloc` feature (jemalloc global
allocator + background purge thread). Build the system-allocator variant with
`cargo build -p musefs --no-default-features` — used for the RSS comparison
(`scripts/rss-churn-bench.sh`) and by packagers that forbid vendored C libs.
```

- [ ] **Step 3: Add the release smoke-build step to `CONTRIBUTING.md`**

In `CONTRIBUTING.md`, under `## Releasing the Rust crates and binaries` →
`**Pre-flight.**`, add a new numbered item after item 3
(`CARGO_REGISTRY_TOKEN is present…`):

```markdown
4. Smoke-build every cross target so `jemalloc-sys` is known to compile under
   zig before tagging (the release matrix builds with the `jemalloc` feature on):

   ```bash
   for t in x86_64-unknown-linux-gnu.2.17 aarch64-unknown-linux-gnu.2.17 \
            x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
     cargo zigbuild --release -p musefs --target "$t"
   done
   ```

   If a target cannot build `jemalloc-sys`, add `--no-default-features` to that
   matrix entry in `release.yml` **and** its matching Docker image, rather than
   blocking the release.
```

- [ ] **Step 4: Commit**

```bash
git add README.md CONTRIBUTING.md
git commit -m "docs: document the jemalloc allocator and its opt-out (#360)"
```

(Docs-only commit — the cargo gate is skipped.)

---

## Task 5: Run the real benchmark and record results in BENCHMARKS.md

**Files:**
- Modify: `BENCHMARKS.md`

- [ ] **Step 1: Run the full benchmark**

Run (defaults: 500 files, nproc workers, 200 samples, 20 warmup — expect ~7 min/variant):
```bash
cp ~/musefs.db /tmp/musefs.db
DB=/tmp/musefs.db scripts/rss-churn-bench.sh | tee /tmp/rss-bench.out
```
Expected: the two-row CSV plus a `decision:` line. Record the two `steady_state_rss_kib` values and the decision.

- [ ] **Step 2: Add the results section to `BENCHMARKS.md`**

Append a new section at the end of `BENCHMARKS.md` (fill the two RSS cells and the decision line from Step 1's output — these are measured runtime values):

```markdown
## Global allocator — steady-state RSS (#360)

Long-lived high-churn FUSE load fragments glibc malloc, growing daemon RSS over
days without a true leak. The `musefs` binary now defaults to the jemalloc
global allocator with a background purge thread. Measured with
`scripts/rss-churn-bench.sh` (Linux; median `VmRSS` over the flattened tail —
steady state, not peak).

**Parameters:** WORKERS=<nproc on this box>, FILES=500, CYCLES=200, WARMUP=20,
no REFRESH_CMD; DB = the ~4427-track store on tmpfs (`/tmp`).

| Allocator      | Steady-state RSS |
| -------------- | ---------------- |
| system malloc  | <measured> MiB   |
| jemalloc       | <measured> MiB   |

**Decision:** <SHIP / INVESTIGATE per the script's decision line> — ship rule is
jemalloc steady-state RSS ≤ system malloc; a within-noise tie is recorded and
decided explicitly here.
```

- [ ] **Step 3: Verify the decision is consistent with the spec gate**

Confirm the recorded decision matches the §4 rule in the spec: ship only if
jemalloc ≤ system malloc (or an explicit within-noise call). If jemalloc is
meaningfully worse, **stop and escalate** — do not proceed to release; the
change is blocked pending investigation (spec Risks table).

- [ ] **Step 4: Commit**

```bash
git add BENCHMARKS.md
git commit -m "docs(benchmarks): record allocator steady-state RSS comparison (#360)"
```

(Docs-only commit — the cargo gate is skipped.)

---

## Task 6: Verify release cross-builds (conditional fix)

**Files:**
- Modify **only if a target fails**: `.github/workflows/release.yml` (+ matching Docker image)

- [ ] **Step 1: Install the cross toolchain (if not already present)**

Run:
```bash
for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
         x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  rustup target add "$t"
done
cargo install cargo-zigbuild --version 0.22.3 || true   # zig must be on PATH (0.13.0)
```
Expected: targets added; `cargo-zigbuild --version` works and `zig version` prints `0.13.0`.

- [ ] **Step 2: Cross-build all four release targets with the jemalloc feature on**

Run:
```bash
for t in x86_64-unknown-linux-gnu.2.17 aarch64-unknown-linux-gnu.2.17 \
         x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  echo "=== $t ==="
  cargo zigbuild --release -p musefs --target "$t"
done
```
Expected: all four link `jemalloc-sys` and produce a binary. This is the spec's primary build-risk surface (jemalloc-sys C under zig cross-compilation).

- [ ] **Step 3: If a target failed, apply the per-target opt-out**

For each failing `<triple>` only, edit `.github/workflows/release.yml`: in the
`build` job's `Build` step, the run line is currently:

```yaml
        run: cargo zigbuild --release -p musefs --target ${{ matrix.zig_target }}
```

Add a per-entry flag in the matrix (add `no_default: true` to the failing
`include` entry) and make the build line honor it, e.g.:

```yaml
        run: cargo zigbuild --release -p musefs --target ${{ matrix.zig_target }} ${{ matrix.no_default && '--no-default-features' || '' }}
```

Then apply the same `--no-default-features` to that arch's Docker image build
(`docker/Dockerfile` / `docker/Dockerfile.musl` cargo invocation) so the tarball
and container for that arch ship the same allocator. Document the dropped target
in `CONTRIBUTING.md` under the release section.

- [ ] **Step 4: If all four built, no workflow change is needed**

Record (in the PR description) that all four targets cross-built `jemalloc-sys`
cleanly, so the conditional fallback was not triggered.

- [ ] **Step 5: Commit (only if Step 3 changed files)**

```bash
git add .github/workflows/release.yml docker/ CONTRIBUTING.md
git commit -m "ci(release): opt the <triple> target out of jemalloc (jemalloc-sys cross-build) (#360)"
```

---

## Self-Review

**Spec coverage:**
- Crate wiring / default-on feature → Task 1 (+ `log` in Task 2). ✔
- `#[global_allocator]`, no-`unsafe` → Task 1 Step 4. ✔
- Background purge thread, best-effort, startup, default decay → Task 2. ✔
- Sanitizers unaffected / native builds → no code action needed (verified in spec §3); release cross-build → Task 6. ✔
- Verification harness (concurrency, file-set, refresh hook, duration, steady-state median, decision rule) → Task 3 (script) + Task 5 (run + record + gate). ✔
- Docs: BENCHMARKS → Task 5; README + CONTRIBUTING (feature matrix + release smoke) → Task 4. ✔
- Risks: jemalloc-sys cross-build → Task 6; background_thread unsupported → Task 2 best-effort + test; RSS no-improvement → Task 5 Step 3 gate. ✔
- Acceptance criteria 1–6 all map to tasks (AC1/2 → T1/T2; AC3 → pre-commit workspace tests + `-p musefs --ignored` under default features; AC4 → T6; AC5 → T3+T5; AC6 → T4). ✔

**Placeholder scan:** the only `<…>` placeholders are empirical benchmark numbers in Task 5 Step 2 (measured at runtime) and the failing-`<triple>` name in Task 6 (conditional, unknown until Step 2 runs) — both unavoidable and clearly marked. No logic/code placeholders.

**Type/name consistency:** `enable_jemalloc_background_thread` (defined T2 Step 4, called T2 Step 4 main + tests T2 Step 2), `GLOBAL` static (T1 Step 4), feature name `jemalloc` and labels `sysmalloc`/`jemalloc` (consistent across Cargo.toml, script `case` arms, BENCHMARKS table). Crate versions pinned 0.7 throughout.
