# StructureOnly FUSE Passthrough Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** StructureOnly reads served by the kernel directly from the backing fd (FUSE passthrough, kernel 6.9+), with silent fallback to the daemon read path everywhere passthrough is unavailable.

**Architecture:** Core gains one accessor (`Musefs::passthrough_fd`) exposing the already-opened-and-validated backing `File` of a handle, `Some` only in `StructureOnly` mode. The FUSE layer advertises `FUSE_PASSTHROUGH` at init, registers the fd at `open` (`reply.open_backing` → `BackingId` → `opened_passthrough`), holds the `BackingId` in an `fh → BackingId` map until `release`, and falls back to the existing `opened` reply on any failure (sticky-disabled after the first).

**Tech Stack:** Rust; fuser 0.17 (already a dependency — `BackingId`, `opened_passthrough`, `set_max_stack_depth`); no new dependencies.

**Spec:** `docs/superpowers/specs/2026-06-06-issue-112-structureonly-passthrough-design.md` — read it before starting. Binding decisions: silent fallback, no CLI flag, POSIX open-time-validation freshness, insert-before-reply ordering, `FOPEN_KEEP_CACHE` stripped on passthrough replies, init must call **both** `add_capabilities(FUSE_PASSTHROUGH)` and `set_max_stack_depth(2)`.

**Conventions that apply (from CLAUDE.md):** Serena tools for code reads/edits; integer-conversion convention (no bare `as`); each crate's `error.rs` owns its errors (no new errors needed here); commit per task; don't push.

---

## File map

| File | Change |
|---|---|
| `musefs-core/src/facade.rs` | Add `PassthroughFd` wrapper + `Musefs::passthrough_fd`; unit test |
| `musefs-core/src/lib.rs` | Re-export `PassthroughFd` |
| `musefs-fuse/src/lib.rs` | `init` capability handshake; `MusefsFs` fields; `open`/`release` wiring |
| `musefs-fuse/tests/passthrough.rs` | New metrics-gated e2e test (passthrough + synthesis control) |
| `musefs-core/src/metrics.rs` | Module-doc note: passthrough reads are invisible to serve counters |
| `CLAUDE.md` | StructureOnly mode bullet: passthrough + freshness semantics |
| `BENCHMARKS.md` | Before/after StructureOnly sequential-read throughput |

---

### Task 1: Core `passthrough_fd` accessor

**Files:**
- Modify: `musefs-core/src/facade.rs` (struct after `Handle` ~line 63, method in `impl Musefs` after `open_handle` ~line 1040, test in the existing `mod tests`)
- Modify: `musefs-core/src/lib.rs:17` (re-export)

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `musefs-core/src/facade.rs` (it models `open_handle_reresolves_after_content_version_bump`, which is the fixture pattern to mirror):

```rust
#[test]
fn passthrough_fd_exposes_backing_only_in_structure_only() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = |mode| MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode,
        poll_interval: std::time::Duration::ZERO,
    };

    // StructureOnly: exposed, and the fd refers to the backing inode.
    let fs = Musefs::open(
        musefs_db::Db::open(&db_path).unwrap(),
        cfg(Mode::StructureOnly),
    )
    .unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();
    let pfd = fs
        .passthrough_fd(fh)
        .expect("StructureOnly exposes the backing fd");
    let fd_meta = std::fs::File::from(pfd.as_fd().try_clone_to_owned().unwrap())
        .metadata()
        .unwrap();
    let backing_meta = std::fs::metadata(dir.path().join("a.mp3")).unwrap();
    assert_eq!(
        (fd_meta.dev(), fd_meta.ino()),
        (backing_meta.dev(), backing_meta.ino()),
        "passthrough fd must be the backing file"
    );

    // A released handle no longer resolves.
    fs.release_handle(fh);
    assert!(fs.passthrough_fd(fh).is_none());

    // Synthesis: never exposed, even for a live handle.
    let fs = Musefs::open(
        musefs_db::Db::open(&db_path).unwrap(),
        cfg(Mode::Synthesis),
    )
    .unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let fh = fs.open_handle(file_inode).unwrap();
    assert!(fs.passthrough_fd(fh).is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core passthrough_fd_exposes`
Expected: FAIL to compile — `no method named passthrough_fd`

- [ ] **Step 3: Write the implementation**

In `musefs-core/src/facade.rs`, insert after the `Handle` struct (after ~line 63):

```rust
/// An owned view of an open handle's backing fd, for FUSE passthrough
/// registration. Holds its own `Arc<Handle>`, so the fd outlives a concurrent
/// slab removal while the registration ioctl is in flight.
pub struct PassthroughFd(Arc<Handle>);

impl std::os::fd::AsFd for PassthroughFd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.file.as_fd()
    }
}
```

(`use std::os::fd::AsFd;` is needed in scope for `self.0.file.as_fd()` — add it to the imports at the top of `facade.rs`.)

In `impl Musefs`, insert after `release_handle` (~line 1046):

```rust
/// The backing fd behind `fh`, for kernel passthrough registration. `Some`
/// only in StructureOnly mode, where the served bytes ARE the backing file;
/// in Synthesis mode the bytes are spliced, so no single fd represents
/// them. `None` also for a stale or released handle.
pub fn passthrough_fd(&self, fh: Fh) -> Option<PassthroughFd> {
    if self.config.mode != Mode::StructureOnly {
        return None;
    }
    let handle = self.handles.get(fh.slab_key())?;
    Some(PassthroughFd(Arc::clone(&handle)))
}
```

(`self.handles.get` returns a `sharded_slab::Entry` that derefs to `Arc<Handle>`; if `Arc::clone(&handle)` fails to infer through the guard, use `Arc::clone(&*handle)`.)

In `musefs-core/src/lib.rs`, extend line 17:

```rust
pub use facade::{Attr, Fh, Mode, MountConfig, Musefs, PassthroughFd};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core passthrough_fd_exposes`
Expected: PASS

- [ ] **Step 5: Run the crate's full test suite (no regressions)**

Run: `cargo test -p musefs-core`
Expected: all PASS

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/src/lib.rs
git commit -m "core: expose a handle's backing fd for passthrough (StructureOnly only) (#112)"
```

---

### Task 2: FUSE init advertises FUSE_PASSTHROUGH

**Files:**
- Modify: `musefs-fuse/src/lib.rs:239-253` (the `init` method of `impl Filesystem for MusefsFs`)

No unit test is possible — `KernelConfig` cannot be constructed outside fuser. The e2e test in Task 4 is the verification; this task is gated on compile + clippy.

- [ ] **Step 1: Add the two init calls**

In `init`, after the existing `FUSE_PARALLEL_DIROPS` line (`musefs-fuse/src/lib.rs:252`), add:

```rust
        // Passthrough needs BOTH calls: fuser only copies max_stack_depth into
        // the init reply when the FUSE_PASSTHROUGH bit was negotiated, and a
        // depth of 0 disables passthrough outright. Depth 2 (fuser's own
        // example value) additionally lets backing files live on a stacked fs
        // (e.g. a music library on overlayfs). On kernels without support the
        // bit is simply not acked and open_backing later fails -> fallback.
        let _ = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH);
        let _ = config.set_max_stack_depth(2);
```

- [ ] **Step 2: Verify it compiles clean**

Run: `cargo clippy -p musefs-fuse --all-targets`
Expected: no errors, no new warnings

- [ ] **Step 3: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "fuse: advertise FUSE_PASSTHROUGH + stack depth at init (#112)"
```

---

### Task 3: FUSE open/release passthrough wiring

**Files:**
- Modify: `musefs-fuse/src/lib.rs` — `MusefsFs` struct (~line 156), `MusefsFs::new` (~line 173), `open` (~line 283), `release` (~line 292), fuser import list (~line 14)

Like Task 2, this is only observable through a real mount; Task 4's e2e test is the verification. Gate on compile + clippy + the existing (non-ignored) suite.

- [ ] **Step 1: Add the fuser import**

Add `BackingId` to the existing `use fuser::{...}` list (`musefs-fuse/src/lib.rs:14-18`). Add `use std::collections::HashMap;` to the std imports.

- [ ] **Step 2: Add the two fields to `MusefsFs`**

After the `poll_pending` field (~line 165):

```rust
    /// Kernel-registered backing fds for live passthrough handles, keyed by
    /// the wire fh. The entry is inserted BEFORE the open reply is sent — the
    /// kernel cannot release an fh it has not yet seen — so every live
    /// passthrough handle has an entry. `release` removes it; dropping the
    /// `BackingId` fires the backing-close ioctl.
    backing: Arc<Mutex<HashMap<u64, BackingId>>>,
    /// Sticky disable: flipped on the first `open_backing` failure (kernel
    /// without passthrough support), so later opens skip the doomed ioctl.
    passthrough_disabled: Arc<AtomicBool>,
```

In `MusefsFs::new` (~line 173), add to the struct literal:

```rust
            backing: Arc::new(Mutex::new(HashMap::new())),
            passthrough_disabled: Arc::new(AtomicBool::new(false)),
```

- [ ] **Step 3: Rewrite `open`**

Replace the body of `fn open` (`musefs-fuse/src/lib.rs:283-290`) with:

```rust
    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let core = Arc::clone(&self.core);
        let flags = open_flags(self.config.keep_cache);
        let backing = Arc::clone(&self.backing);
        let passthrough_disabled = Arc::clone(&self.passthrough_disabled);
        self.pool.execute(move || {
            let fh = match core.open_handle(ino.0) {
                Ok(fh) => fh,
                Err(e) => return reply.error(reply_errno("open", ino.0, &e)),
            };
            if !passthrough_disabled.load(Ordering::Relaxed) {
                if let Some(pfd) = core.passthrough_fd(fh) {
                    match reply.open_backing(&pfd) {
                        Ok(id) => {
                            // Insert before the reply (see the `backing` field
                            // doc). FOPEN_KEEP_CACHE is dropped: page-cache
                            // ownership belongs to the backing inode here.
                            // Poisoning recovery: the lock guards single map
                            // ops; a panic mid-insert leaves nothing torn.
                            let mut map = backing
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let id = map.entry(fh.get()).or_insert(id);
                            return reply.opened_passthrough(
                                FileHandle(fh.get()),
                                FopenFlags::empty(),
                                id,
                            );
                        }
                        Err(e) => {
                            // Sticky: the failure modes (kernel < 6.9, ioctl
                            // unsupported) are static per mount.
                            passthrough_disabled.store(true, Ordering::Relaxed);
                            log::info!(
                                "FUSE passthrough unavailable; serving reads through the daemon: {e}"
                            );
                        }
                    }
                }
            }
            reply.opened(FileHandle(fh.get()), flags);
        });
    }
```

- [ ] **Step 4: Extend `release`**

In `fn release` (`musefs-fuse/src/lib.rs:292-307`), replace the `if let` body:

```rust
        // Cheap (two map removes); no need to offload to the pool.
        if let Some(fh) = NonZeroU64::new(fh.0) {
            // Dropping the BackingId fires the backing-close ioctl. Absent for
            // plain (non-passthrough) handles — remove is then a no-op.
            self.backing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&fh.get());
            self.core.release_handle(Fh::from(fh));
        }
        reply.ok();
```

- [ ] **Step 5: Verify compile + existing suite**

Run: `cargo clippy -p musefs-fuse --all-targets && cargo test -p musefs-fuse`
Expected: clean clippy; all non-ignored tests PASS

- [ ] **Step 6: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "fuse: register StructureOnly backing fds for kernel passthrough (#112)"
```

---

### Task 4: End-to-end passthrough test

**Files:**
- Create: `musefs-fuse/tests/passthrough.rs`

Two `#[ignore]`d tests in one metrics-gated binary: the passthrough assertion, and a Synthesis control that proves the pread counter observable is live (guarding the zero-assert against vacuity). Metrics counters are process-global, so the run command pins `--test-threads=1` and each test calls `reset()`.

- [ ] **Step 1: Write the test file**

```rust
#![cfg(feature = "metrics")]
//! StructureOnly passthrough: after `open`, the kernel must serve reads
//! directly from the registered backing fd — byte-identical content with ZERO
//! daemon preads. The Synthesis control test proves the pread counter
//! observable is live (a broken counter would make the zero-assert vacuous).
//!
//! Run with:
//!   cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1

use std::collections::BTreeMap;
use std::io::Read;

use musefs_core::{metrics, scan_directory, Mode, MountConfig, Musefs};

// ---------------------------------------------------------------------------
// Minimal proven FLAC fixture (mirrors tests/mount.rs exactly)
// ---------------------------------------------------------------------------

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("orig", comments), true));
    out.extend_from_slice(audio);
    out
}

fn config(mode: Mode) -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode,
        poll_interval: std::time::Duration::ZERO,
    }
}

/// FUSE passthrough landed in mainline 6.9.
fn kernel_supports_passthrough() -> bool {
    let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = rel.trim().split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor) >= (6, 9)
}

/// Scan one ~2 MiB FLAC into a fresh on-disk DB and mount it. Returns the
/// backing bytes, the virtual path, the session, and the two TempDir guards.
fn mount_one_track(
    mode: Mode,
) -> (
    Vec<u8>,
    std::path::PathBuf,
    fuser::BackgroundSession,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let backing = tempfile::tempdir().unwrap();
    let audio = vec![0xABu8; 2 * 1024 * 1024];
    let flac = make_flac(&["ARTIST=Alpha", "TITLE=Track"], &audio);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();

    // On-disk DB so musefs_db uses the PerThread pool (mirrors tests/mount.rs).
    let db_path = backing.path().join("m.db");
    let db = musefs_db::Db::open(&db_path).unwrap();
    scan_directory(&db, backing.path()).unwrap();

    let fs = Musefs::open(db, config(mode)).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-passthrough-test").unwrap();
    let mnt = mountpoint.keep(); // keep mount alive for the test's duration
    let virt = mnt.join("Alpha").join("Track.flac");
    (flac, virt, session, backing, mnt)
}

#[test]
#[ignore = "real mount; needs /dev/fuse + kernel >= 6.9 — run with: cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1"]
fn structure_only_reads_are_kernel_passthrough() {
    if !kernel_supports_passthrough() {
        eprintln!("kernel < 6.9: no FUSE passthrough; skipping");
        return;
    }
    let (backing_bytes, virt, session, _backing, _mnt) = mount_one_track(Mode::StructureOnly);

    // Sequencing matters: FUSE `open` fires here (on_open and warmup counters
    // land), THEN reset, THEN read — so the pread assertion has a clean
    // baseline and covers exactly the reads of this fd.
    let mut f = std::fs::File::open(&virt).expect("open through mount");
    metrics::reset();
    let mut served = Vec::new();
    f.read_to_end(&mut served).expect("read through mount");

    assert_eq!(
        served, backing_bytes,
        "StructureOnly must serve backing bytes verbatim"
    );
    let snap = metrics::snapshot();
    assert_eq!(
        snap.preads, 0,
        "daemon served {} preads — kernel passthrough did not engage",
        snap.preads
    );

    // Close + unmount exercise the BackingId release path (release drops the
    // map entry; session drop tears down the channel) — must not hang or error.
    drop(f);
    drop(session);
}

#[test]
#[ignore = "real mount; needs /dev/fuse — run with: cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1"]
fn synthesis_reads_still_go_through_the_daemon() {
    let (_backing_bytes, virt, session, _backing, _mnt) = mount_one_track(Mode::Synthesis);

    let mut f = std::fs::File::open(&virt).expect("open through mount");
    metrics::reset();
    let mut served = Vec::new();
    f.read_to_end(&mut served).expect("read through mount");

    // Synthesis splices a fresh header; the read MUST hit the daemon. This
    // proves the pread counter observable is live, so the passthrough test's
    // zero-assert cannot pass vacuously.
    let snap = metrics::snapshot();
    assert!(
        snap.preads > 0,
        "expected daemon preads on a Synthesis mount; the metrics observable is broken"
    );
    drop(f);
    drop(session);
}
```

- [ ] **Step 2: Run the new tests (they must FAIL only if Tasks 1–3 are broken)**

Run: `cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1`
Expected: both PASS (kernel here is 7.0). If `structure_only_reads_are_kernel_passthrough` fails on the pread assert, passthrough did not engage — debug Tasks 2/3 (most likely the init handshake) before proceeding; do not weaken the assert.

- [ ] **Step 3: Run the full ignored e2e suite (no regressions)**

Run: `cargo test -p musefs-fuse --features metrics -- --ignored --nocapture --test-threads=1`
Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add musefs-fuse/tests/passthrough.rs
git commit -m "fuse: e2e test for StructureOnly kernel passthrough (#112)"
```

---

### Task 5: Documentation

**Files:**
- Modify: `musefs-core/src/facade.rs:17-25` (the `Mode` doc comments)
- Modify: `musefs-core/src/metrics.rs:1-19` (module doc)
- Modify: `CLAUDE.md` (the `StructureOnly` mode bullet)

- [ ] **Step 1: `Mode::StructureOnly` doc comment**

Replace the `StructureOnly` variant doc in `musefs-core/src/facade.rs`:

```rust
    /// Pure passthrough: serve the original backing file bytes unchanged.
    /// Where the kernel supports FUSE passthrough (6.9+), reads are served
    /// directly from the backing fd registered at open — open-time validation
    /// only: a handle held across a backing-file replacement keeps serving
    /// the inode it opened (plain POSIX fd semantics); new opens re-resolve.
    StructureOnly,
```

- [ ] **Step 2: metrics.rs module-doc note**

Append to the "Counting scope" paragraph in `musefs-core/src/metrics.rs` (after the `on_scan_open`/`on_scan_read` sentence):

```rust
//! Counters measure *daemon* work, not user traffic: StructureOnly reads
//! served via kernel passthrough never reach userspace and are invisible to
//! `on_pread` — by design (the passthrough e2e test asserts exactly this).
```

- [ ] **Step 3: CLAUDE.md mode bullet**

In CLAUDE.md's "Two mount **modes**" section, extend the `StructureOnly` bullet:

```markdown
- `StructureOnly` — a single whole-file `BackingAudio` segment; the original bytes
  are served verbatim under the templated tree. Stored audio bounds are not
  validated in this mode because the whole file is served. On kernels with FUSE
  passthrough (6.9+) reads are served by the kernel directly from the backing fd
  registered at open (silent fallback to daemon reads elsewhere); freshness is
  open-time-only for such handles — plain POSIX fd semantics.
```

- [ ] **Step 4: Verify build (doc comments compile)**

Run: `cargo clippy --all-targets`
Expected: clean

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/src/metrics.rs CLAUDE.md
git commit -m "docs: StructureOnly passthrough semantics (#112)"
```

---

### Task 6: Benchmark and BENCHMARKS.md entry

**Files:**
- Modify: `BENCHMARKS.md` (new section)

Before = `main` (daemon-served reads), After = this branch (kernel passthrough). Same machine, same fixture, release builds. The backing file is RAM-cached either way (17 GiB RAM vs a 512 MiB file), so the measurement isolates FUSE-path overhead — which is the thing the issue is about.

- [ ] **Step 1: Generate the fixture (one 512 MiB FLAC)**

```bash
mkdir -p /tmp/pt-bench/backing /tmp/pt-bench/mnt
python3 - <<'EOF'
import struct
def block(btype, body, last=False):
    h = bytes([ (0x80 if last else 0) | btype ]) + len(body).to_bytes(3, 'big')
    return h + body
streaminfo = bytes([0x10,0,0x10,0,0,0,0,0,0,0,0x0A,0xC4,0x42,0xF0,0,0,0,0]) + bytes(16)
def vc(vendor, comments):
    out = struct.pack('<I', len(vendor)) + vendor.encode()
    out += struct.pack('<I', len(comments))
    for c in comments:
        out += struct.pack('<I', len(c)) + c.encode()
    return out
with open('/tmp/pt-bench/backing/big.flac', 'wb') as f:
    f.write(b'fLaC')
    f.write(block(0, streaminfo))
    f.write(block(4, vc('orig', ['ARTIST=Alpha', 'TITLE=Big']), last=True))
    f.write(bytes([0xAB]) * (512 * 1024 * 1024))
EOF
```

- [ ] **Step 2: Build both binaries**

```bash
cargo build --release   # branch ("after") -> target/release/musefs
git worktree add /tmp/pt-bench/main-tree main
(cd /tmp/pt-bench/main-tree && cargo build --release)   # "before"
```

- [ ] **Step 3: Measure (3 runs each, fresh mount per run)**

For each binary `BIN` in `target/release/musefs` (after) and `/tmp/pt-bench/main-tree/target/release/musefs` (before):

```bash
"$BIN" scan /tmp/pt-bench/backing --db /tmp/pt-bench/m.db
"$BIN" mount /tmp/pt-bench/mnt --db /tmp/pt-bench/m.db --mode structure-only &
MOUNT_PID=$!; sleep 1
for i in 1 2 3; do
  dd if=/tmp/pt-bench/mnt/Alpha/Big.flac of=/dev/null bs=1M 2>&1 | tail -1
done
fusermount3 -u /tmp/pt-bench/mnt; wait $MOUNT_PID
```

Record the dd-reported MB/s for each run. (First run includes page-cache warmup of the backing file; report all three, call out the median.)

- [ ] **Step 4: Record in BENCHMARKS.md and clean up**

Append a section following the existing format (Before/After bullets, exact commands, measured date):

```markdown
## Issue #112 — StructureOnly kernel passthrough

*Measured YYYY-MM-DD.*

- **Before** = `main` @ `<sha>`: every read round-trips kernel -> daemon -> positioned read -> copy back.
- **After** = `issue-112-passthrough`: backing fd registered at open (FUSE passthrough, kernel 6.9+); the kernel serves reads directly.
- Harness: 512 MiB single-track StructureOnly mount, `dd bs=1M` sequential read, 3 runs each, fresh mount per run, RAM-cached backing file (isolates FUSE-path overhead).

| | run 1 | run 2 | run 3 | median |
|---|---|---|---|---|
| Before (daemon reads) | _MB/s_ | _MB/s_ | _MB/s_ | _MB/s_ |
| After (passthrough) | _MB/s_ | _MB/s_ | _MB/s_ | _MB/s_ |
```

(The `_MB/s_` cells are filled with the measured numbers from Step 3 — do not commit placeholders.)

Clean up: `git worktree remove /tmp/pt-bench/main-tree && rm -rf /tmp/pt-bench`

- [ ] **Step 5: Commit**

```bash
git add BENCHMARKS.md
git commit -m "bench: StructureOnly passthrough before/after throughput (#112)"
```

---

### Task 7: Final validation

- [ ] **Step 1: Format + lint + full workspace tests**

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test
```
Expected: all clean/PASS. (`cargo fmt --all --check` must exit 0 — CI has a fmt gate; check the exit status directly, don't pipe.)

- [ ] **Step 2: FUSE e2e suite once more**

```bash
cargo test -p musefs-fuse --features metrics -- --ignored --nocapture --test-threads=1
```
Expected: all PASS

- [ ] **Step 3: In-diff mutation gate (CI parity)**

Run exactly as documented in CLAUDE.md (memory-capped cgroup, TMPDIR on real disk, sanity-check the diff first):

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
mkdir -p ~/.cache/musefs-mutants-tmp
TMPDIR="$HOME/.cache/musefs-mutants-tmp" systemd-run --user --scope --collect \
    -p MemoryMax=10G -p MemorySwapMax=0 \
    cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```
Expected: exit 0, no missed mutants. Note: the FUSE handler bodies (`open`/`release`/`init`) are only exercised by `#[ignore]`d tests, which cargo-mutants does not run — mutants there may survive. Survivors confined to those handlers are acceptable with a note in the PR; survivors in `facade.rs` are not (the unit test must catch them — strengthen it instead).

- [ ] **Step 4: Wrap up**

Use superpowers:finishing-a-development-branch to choose merge/PR handling. PR title: `StructureOnly: serve reads via kernel FUSE passthrough (#112)`.

---

## Self-review notes (already applied)

- Spec coverage: init handshake (Task 2), accessor + mode gating (Task 1), open/release lifecycle with insert-before-reply and KEEP_CACHE strip (Task 3), silent fallback + sticky disable (Task 3), e2e with metrics sequencing + kernel skip + synthesis control (Task 4), docs (Task 5), benchmark (Task 6), mutation gate (Task 7). Freshness semantics need no code — documented in Task 5.
- Type consistency: `passthrough_fd(fh: Fh) -> Option<PassthroughFd>` used identically in Tasks 1 and 3; map keyed by `fh.get()` (wire value) in both `open` and `release`.
- No placeholders: benchmark table cells are explicitly filled at measurement time (Step 4 says so); every code step shows complete code.
