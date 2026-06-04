# `Db<Mode>` Typestate Implementation Plan (#130)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Distinguish read-only from writable DB connections at the type level so a write API on a read-only connection — or a serve path that writes — fails to compile.

**Architecture:** `musefs_db::Db` becomes `Db<M = ReadWrite>` with marker types. Read methods live on `impl<M> Db<M>`, write methods on `impl Db<ReadWrite>`; the `ReadWrite` default keeps every existing call-site spelling (`Db`, `Db::open`, `&Db`) compiling unchanged. In musefs-core, `DbPool` degrades the mount connection to `Db<ReadOnly>` and hands out `&Db<ReadOnly>`; the 12 serve-path fns in `reader.rs`/`facade.rs`/`mapping.rs` become generic over `&Db<M>` (NOT concrete `&Db<ReadOnly>` — `Musefs::open` calls `build_full` on the writable connection before pooling it, and integration tests pass writable in-memory DBs into `resolve`/`read_at`). Write methods don't resolve for an unconstrained `M`, so the serve-path bodies provably contain no write. Purely type-level: zero runtime change.

**Tech Stack:** Rust (`PhantomData` typestate), rusqlite, parking_lot.

**Spec:** `docs/superpowers/specs/2026-06-04-format-strum-db-typestate-design.md` (Part 2)

**Key compile fact used throughout:** type-parameter defaults apply in *type position* (`impl Db`, `db: &Db`, `-> Result<Db>` all mean `Db<ReadWrite>`), while *expression-position* paths infer (`Db::open_readonly(p)` resolves `M = ReadOnly` because the fn exists only on that impl). This is why untouched files keep compiling.

---

### Task 1: Typestate core in `musefs-db/src/lib.rs`

**Files:**
- Modify: `musefs-db/src/lib.rs`
- Test: doctests on `Db` (in the same file) + existing `lib.rs` tests

- [ ] **Step 1: Create the branch**

```bash
git checkout -b db-typestate main
```

- [ ] **Step 2: Write the guarantee as paired doctests (the failing test)**

In `musefs-db/src/lib.rs`, the struct currently reads:

```rust
pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
}
```

Replace it with the generic struct, markers, and doctests:

```rust
/// Type-state markers for [`Db`]: the connection's write capability, at the
/// type level. Write APIs exist only on `Db<ReadWrite>`.
pub struct ReadOnly;
pub struct ReadWrite;

/// A SQLite connection whose mode parameter says whether write APIs resolve.
///
/// Read methods are available in both modes; write methods only on
/// `Db<ReadWrite>` (the default, so `Db` spelled bare means writable):
///
/// ```
/// let db = musefs_db::Db::open_in_memory().unwrap().into_read_only();
/// db.data_version().unwrap();
/// ```
///
/// ```compile_fail
/// let db = musefs_db::Db::open_in_memory().unwrap().into_read_only();
/// db.upsert_track(unimplemented!());
/// ```
pub struct Db<M = ReadWrite> {
    conn: Connection,
    path: Option<PathBuf>,
    _mode: PhantomData<M>,
}
```

Add `use std::marker::PhantomData;` to the imports (after `use rusqlite::Connection;`).

The paired doctests are deliberate: `compile_fail` passes on *any* compile error, so the sibling passing test with the identical `Db<ReadOnly>` spelling proves the failure is specifically the write-method non-resolution, not a typo.

- [ ] **Step 3: Split the `impl Db` block in lib.rs**

The current single `impl Db` block (lines 27-95) holds `open`, `open_in_memory`, `configure`, `user_version`, `data_version`, `path`, `open_readonly`. Split it into three:

```rust
impl Db<ReadWrite> {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Db> {
        let p = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&p)?;
        Self::configure(&mut conn, true)?;
        Ok(Db {
            conn,
            path: Some(p),
            _mode: PhantomData,
        })
    }

    pub fn open_in_memory() -> Result<Db> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&mut conn, false)?;
        Ok(Db {
            conn,
            path: None,
            _mode: PhantomData,
        })
    }

    // configure() moves here verbatim (doc comment and body unchanged).

    /// Degrade to the read-only surface, keeping the same connection. The
    /// change is type-level only — runtime behavior is unchanged. The only
    /// intended caller is `musefs_core`'s `DbPool::new`, which strips write
    /// access from the mount connection before the serve path can see it.
    pub fn into_read_only(self) -> Db<ReadOnly> {
        Db {
            conn: self.conn,
            path: self.path,
            _mode: PhantomData,
        }
    }
}

impl Db<ReadOnly> {
    // open_readonly() moves here verbatim (doc comment unchanged), with the
    // construction gaining `_mode: PhantomData` and the return type written
    // as Result<Db<ReadOnly>>.
}

impl<M> Db<M> {
    // user_version(), data_version(), path() move here verbatim.
}
```

Also update the `mutants`-feature `Default` impl (line ~98): the header `impl Default for Db` stays (type position — it already means `Db<ReadWrite>`); only its constructor gains `_mode: PhantomData`.

- [ ] **Step 4: Build and run musefs-db tests (unit + doctests)**

```bash
cargo test -p musefs-db
```

Expected: PASS, including both doctests — the not-yet-split `impl Db` blocks in the other modules already mean `impl Db<ReadWrite>` via the default, so `upsert_track` is already absent from `Db<ReadOnly>` and the `compile_fail` doctest holds from this commit onward. All four existing `lib.rs` tests (`open_uses_wal_and_busy_timeout`, etc.) pass unchanged. If anything fails to build, a construction site is missing `_mode: PhantomData`.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/lib.rs
git commit -m "musefs-db: Db<Mode> typestate core (#130)"
```

### Task 2: Split `tracks.rs` into read and write surfaces

**Files:**
- Modify: `musefs-db/src/tracks.rs` (the `impl Db` block at line 46)

- [ ] **Step 1: Split the impl block**

`tracks.rs:46` has one `impl Db` block. Split it into two, moving each method **verbatim** (signatures and bodies untouched — only which block they sit in changes):

```rust
impl<M> Db<M> {
    // READ surface — moves here:
    // get_track, get_track_by_path, list_tracks, track_content_version,
    // begin_read, end_read, list_render_keys, changelog_since,
    // render_keys_for
}

impl Db<ReadWrite> {
    // WRITE surface — moves here:
    // upsert_track, delete_track, set_format_for_test,
    // delete_changelog_through_for_test
}
```

Add `ReadWrite` to the file's `use crate::...` import of `Db`. `begin_read`/`end_read` go on the READ surface deliberately: they run `BEGIN`/`ROLLBACK` but are called inside the serve path (`facade.rs:914/922`) under `pool.with`, so they must be reachable from `Db<ReadOnly>`.

- [ ] **Step 2: Build and test**

```bash
cargo test -p musefs-db
```

Expected: PASS. (The crate compiles between tasks because unsplit `impl Db` blocks in other files still mean `Db<ReadWrite>`.)

- [ ] **Step 3: Commit**

```bash
git add musefs-db/src/tracks.rs
git commit -m "musefs-db: split tracks.rs read/write surfaces (#130)"
```

### Task 3: Split `tags.rs`, `art.rs`, `structural.rs`, `bulk.rs`

**Files:**
- Modify: `musefs-db/src/tags.rs` (impl at line 5)
- Modify: `musefs-db/src/art.rs` (impl at line 16)
- Modify: `musefs-db/src/structural.rs` (impl at line 5)
- Modify: `musefs-db/src/bulk.rs` (impl at line 16)

- [ ] **Step 1: Split each impl block** (same mechanics as Task 2 — methods move verbatim, imports gain `ReadWrite`):

| File | `impl<M> Db<M>` (read) | `impl Db<ReadWrite>` (write) |
|---|---|---|
| `tags.rs` | `get_tags`, `tags_for_tracks`, `tags_grouped`, `get_binary_tags`, `read_binary_tag_chunk_into`, `read_binary_tag_chunk` | `replace_tags`, `set_binary_tags` |
| `art.rs` | `get_art`, `get_art_meta`, `read_art_chunk_into`, `read_art_chunk`, `get_track_art` | `upsert_art`, `set_track_art`, `gc_orphan_art` |
| `structural.rs` | `track_ids_with_structural_blocks`, `get_structural_blocks` | `set_structural_blocks` |
| `bulk.rs` | — (nothing) | `apply_bulk_pragmas` (the `pub(crate)` associated fn), `apply_bulk_pragmas_self`, `bulk_writer` |

`bulk.rs` needs no read block — the whole existing `impl Db` block just becomes `impl Db<ReadWrite>`. `BulkWriter` itself is untouched (it holds a `Transaction`, not a `Db`).

- [ ] **Step 2: Build and test**

```bash
cargo test -p musefs-db
```

Expected: PASS, including both doctests from Task 1.

- [ ] **Step 3: Commit**

```bash
git add musefs-db/src/tags.rs musefs-db/src/art.rs musefs-db/src/structural.rs musefs-db/src/bulk.rs
git commit -m "musefs-db: split remaining read/write surfaces (#130)"
```

### Task 4: musefs-core — degrade the pool, genericize the serve path

These two halves land as ONE commit: after the pool change alone, core's serve fns (still `&Db` = `&Db<ReadWrite>`) reject the pool's `&Db<ReadOnly>` and the crate doesn't compile.

**Files:**
- Modify: `musefs-core/src/db_pool.rs`
- Modify: `musefs-core/src/reader.rs:214,239,423,446,457,563,575`
- Modify: `musefs-core/src/mapping.rs:32,67,83`
- Modify: `musefs-core/src/facade.rs:237,264`

- [ ] **Step 1: db_pool.rs holds and hands out `Db<ReadOnly>`**

Change the import (`db_pool.rs:21`):

```rust
use musefs_db::{Db, ReadOnly};
```

The enum (line 38), thread-local (line 62), and the three methods change types; `Drop` is untouched. New versions (doc comments stay as they are today):

```rust
pub enum DbPool {
    PerThread {
        id: u64,
        path: PathBuf,
        poll: ReentrantMutex<Db<ReadOnly>>,
    },
    Shared(Arc<ReentrantMutex<Db<ReadOnly>>>),
}
```

```rust
thread_local! {
    static PER_PATH: RefCell<HashMap<(PathBuf, u64), Rc<Db<ReadOnly>>>> = RefCell::new(HashMap::new());
}
```

`new` keeps its `db: Db` (writable) signature and degrades first — the only behavioral-looking change, and it's type-level only:

```rust
    pub fn new(db: Db) -> Result<DbPool> {
        let db = db.into_read_only();
        match db.path() {
            Some(p) => Ok(DbPool::PerThread {
                id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
                path: p.to_path_buf(),
                poll: ReentrantMutex::new(db),
            }),
            None => Ok(DbPool::Shared(Arc::new(ReentrantMutex::new(db)))),
        }
    }
```

`with_poll` and `with` change only their closure parameter type — bodies stay byte-identical (`Db::open_readonly(path)` inside `with` already produces `Db<ReadOnly>` by inference):

```rust
    pub fn with_poll<R>(&self, f: impl FnOnce(&Db<ReadOnly>) -> Result<R>) -> Result<R> {
```

```rust
    pub fn with<R>(&self, f: impl FnOnce(&Db<ReadOnly>) -> Result<R>) -> Result<R> {
```

- [ ] **Step 2: Genericize the 12 serve-path fns**

In each, add `<M>` to the generics and change `db: &Db` to `db: &Db<M>`. Nothing else in the signatures or bodies changes. The full list:

`musefs-core/src/reader.rs`:

```rust
    pub fn resolve<M>(&self, db: &Db<M>, track_id: i64) -> Result<Arc<ResolvedFile>> {        // line 214
    fn build<M>(&self, db: &Db<M>, track: &musefs_db::Track, meta: &std::fs::Metadata) -> Result<Arc<ResolvedFile>> {  // line 239
pub fn read_at_into<M>(resolved: &ResolvedFile, db: &Db<M>, offset: u64, size: u64, out: &mut Vec<u8>) -> Result<()> {  // line 423
pub fn read_at<M>(resolved: &ResolvedFile, db: &Db<M>, offset: u64, size: u64) -> Result<Vec<u8>> {  // line 446
fn read_segments_into<M>(resolved: &ResolvedFile, db: &Db<M>, file: Option<&std::fs::File>, offset: u64, size: u64, out: &mut Vec<u8>) -> Result<()> {  // line 457
pub fn read_at_with_file_into<M>(resolved: &ResolvedFile, db: &Db<M>, file: &std::fs::File, offset: u64, size: u64, out: &mut Vec<u8>) -> Result<()> {  // line 563
pub fn read_at_with_file<M>(resolved: &ResolvedFile, db: &Db<M>, file: &std::fs::File, offset: u64, size: u64) -> Result<Vec<u8>> {  // line 575
```

(Where a fn's parameters are listed one-per-line in the source, keep that formatting — only the generic and the `db` parameter's type change.)

`musefs-core/src/mapping.rs`:

```rust
pub(crate) fn track_art_to_inputs<M>(db: &Db<M>, track_id: i64) -> Result<Vec<ArtInput>> {     // line 32
pub(crate) fn binary_tags_to_inputs<M>(db: &Db<M>, track_id: i64) -> Result<Vec<BinaryTagInput>> {  // line 67
pub(crate) fn track_art_images<M>(db: &Db<M>, inputs: &[ArtInput]) -> Result<Vec<Vec<u8>>> {   // line 83
```

`musefs-core/src/facade.rs` (both are private associated fns of `Musefs`; add `<M>` after the fn name):

```rust
    fn render_entries<M>(db: &Db<M>, ...) // line 236-237 — rest of the signature unchanged
    fn build_full<M>(db: &Db<M>, ...)     // line 263-264 — rest of the signature unchanged
```

Do NOT touch `scan.rs` — its `&Db` params correctly stay `Db<ReadWrite>` (it writes), and `Musefs::open(db: Db, ...)` correctly keeps taking the writable connection (it runs reads on it, passes it to `build_full` — instantiating `M = ReadWrite` — then moves it into `DbPool::new`).

- [ ] **Step 3: Build and test core**

```bash
cargo test -p musefs-core
```

Expected: PASS with **zero test-file edits** — that absence is itself spec verification ("every existing test spelling compiles as-is"). Tests pass writable in-memory DBs into `resolve`/`read_at` (e.g. `tests/read_at.rs:37`) — the generic accepts them; `reader.rs:1037`'s manual `Db::open_readonly` now yields an honest `Db<ReadOnly>` that the same generic accepts. If a compile error names a fn taking `&Db` receiving a `&Db<ReadOnly>`, that fn was missed in Step 2 — genericize it the same way rather than inserting any conversion.

- [ ] **Step 4: Build the rest of the workspace**

```bash
cargo test -p musefs-fuse && cargo test -p musefs-cli && cargo test -p musefs
```

Expected: PASS untouched (they only ever spell `Db` = `Db<ReadWrite>`).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/db_pool.rs musefs-core/src/reader.rs musefs-core/src/mapping.rs musefs-core/src/facade.rs
git commit -m "musefs-core: serve path takes Db<ReadOnly>; helpers generic over mode (#130)"
```

### Task 5: Workspace validation and mutation gate

**Files:** none modified — verification only.

- [ ] **Step 1: Full workspace check**

```bash
cargo test --workspace && cargo clippy --all-targets && cargo fmt --all --check
```

Expected: all pass (this also runs the FLAC/MP3/etc. proptests via feature unification). Check each exit status directly.

- [ ] **Step 2: FUSE end-to-end (real mounts)**

```bash
cargo test -p musefs-fuse -- --ignored
```

Expected: PASS (needs `/dev/fuse`; this change touches the serve path's types, so run the real-mount suite even though behavior is provably unchanged).

- [ ] **Step 3: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `grep` succeeds (non-empty diff — an empty diff is a silent false pass), `cargo mutants` exits 0. Do NOT set TMPDIR. Note: the diff is mostly signature/impl-header churn, which yields few mutants — that's expected, not a gate failure.

- [ ] **Step 4: Hand off**

The branch is ready for review/merge — use the superpowers:finishing-a-development-branch skill (PR title: `Db<Mode> typestate: read-only vs writable connections at the type level (#130)`).
