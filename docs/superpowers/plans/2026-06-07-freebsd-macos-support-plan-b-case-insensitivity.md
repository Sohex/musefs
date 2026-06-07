# FreeBSD/macOS Support — Plan B: Runtime Case-Insensitivity (Tree Case-Folding)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an OS-defaulted, user-overridable `case_insensitive` mode so that on a case-insensitive volume (macOS) the virtual tree folds case: case-variant directories **merge** into one, case-variant leaf files **disambiguate**, and lookups match regardless of case — while Linux/FreeBSD behavior stays byte-for-byte identical and pays zero overhead.

**Architecture:** A single `case_insensitive` flag lives on `MountConfig`, is set by a new `--case-insensitive` CLI flag (default `true` on macOS, `false` elsewhere), and is stored on `VirtualTree`. When set, name comparisons fold through a build-only folded-name index (`parent → fold(name) → inode`) populated at the two insert sites. **The incremental refresh path is bypassed when folding** (`rebuild_incremental` returns its existing `Ok(None)`, so the caller does a full folded `build_with`) — this keeps correctness without folding the exact-rendered-name navigation in `apply_changes`. The acceptance gate is folded full-build correctness plus a facade refresh test.

**Tech Stack:** Rust, `im` persistent maps (`ImHashMap`/`OrdMap`), `clap`.

**Spec:** `docs/superpowers/specs/2026-06-07-freebsd-macos-support-design.md` (§2)

**Depends on:** Plan A is independent; this plan can land before or after it. The two share only the CLI/config surface and do not conflict.

---

## Design decisions (read before starting)

- **Fold function:** `str::to_lowercase()` (Unicode-aware lowercasing). ASCII case-insensitivity is the floor; full Apple HFS+/APFS normalization (NFD + Apple's folding table) is out of scope for this best-effort pass. Apply the fold *identically* to every comparison (collision, disambiguation, lookup) — never compare a folded value against an unfolded one.
- **Directories merge, leaf files disambiguate.** `ensure_dir` reuses an existing *directory* child matching the folded name (merge); `disambiguate` treats a folded collision as a collision and appends ` (k)` (leaf disambiguation). Mirrors a native case-insensitive filesystem; minimal generalization of the existing exact-name behavior.
- **Incremental refresh bypassed when folding.** `apply_changes` and its helpers (`deepest_existing_ancestor`, `children_by_rendered`, `collision_gate`) navigate by *exact* rendered name and would mis-handle a merged folded tree, silently breaking the fresh-vs-incremental equivalence invariant. Rather than fold that hard-to-verify path, `rebuild_incremental` returns `Ok(None)` when `case_insensitive` is set; the existing caller (`poll_refresh_notify`, facade.rs:665) already treats `Ok(None)` as "do a full rebuild" via `rebuild_full`. So `collision_gate`, `deepest_existing_ancestor`, `children_by_rendered`, `apply_changes`, `remove_track`, and `prune_empty_dirs_upward` are **left untouched** — under folding they are never invoked.
- **Folded index is build-only.** Because folding always full-rebuilds, a published folded tree is never mutated in place — `build_with` only ever *inserts*. So only the two insert sites maintain `folded_children`; there is no remove-side maintenance to get wrong. The case-sensitive path never populates the index (the maintenance helper early-returns), so Linux/FreeBSD pay nothing. The index is part of `VirtualTree`'s derived `PartialEq`; since it is built deterministically from the same inserts, a fresh folded build and a re-rendered folded build compare equal.
- **No mount-option change.** Case-insensitivity is enforced entirely by the daemon's folded lookup. macFUSE/FUSE-T do not honor a daemon-set case-sensitivity mount flag, so `platform::mount::options` (Plan A) is unchanged — matching the spec's "where the backend honors it" hedge.
- **Minimal-churn constructor.** `build_with(entries, alloc)` stays 2-arg (case-sensitive) and delegates to a new `build_with_ci(entries, alloc, case_insensitive)`. This avoids touching the ~30 existing `build_with` call sites in `tree.rs`'s own tests; only the 5 production call sites in `facade.rs` move to `build_with_ci`.

---

## File Structure

- Modify: `musefs-core/src/tree.rs` — add `case_insensitive` + `folded_children` fields; add `fold`, `taken`, `dir_child_named`, `insert_folded_child` helpers; fold `disambiguate`, `ensure_dir` (merge), `lookup`; add `build_with_ci` (and make `build_with` delegate); new unit tests.
- Modify: `musefs-core/src/facade.rs` — add `case_insensitive` to `MountConfig`; bypass `rebuild_incremental` when folding; move the 5 production `build_with` calls to `build_with_ci`.
- Modify: `musefs-cli/src/lib.rs` — add `--case-insensitive` flag (OS-defaulted) to `MountArgs`; set it in `parse_mount_config`.
- Modify: every `MountConfig { ... }` literal across the workspace (≈30, mostly test helpers) — add `case_insensitive: false,`.
- Modify: `musefs-core/tests/incremental_refresh.rs` — add the case-insensitive refresh acceptance test.
- Modify: `README.md` — document the flag.

---

## Task 1: Tree case-folding (build path) + flag plumbing

Everything here is gated on `case_insensitive`, which defaults to `false`
everywhere in this task, so the full suite stays green and Linux/FreeBSD behavior
is byte-identical. Folded behavior is exercised in Task 2.

**Files:**
- Modify: `musefs-core/src/tree.rs`
- Modify: `musefs-core/src/facade.rs`
- Modify: all `MountConfig { ... }` literals (compiler-guided)

- [ ] **Step 1: Add the fold helper**

In `musefs-core/src/tree.rs`, add this free function just above
`impl InodeAllocator` (after the `InodeAllocator` struct, ~line 22):

```rust
/// Case-fold a name for case-insensitive comparison. Unicode-aware lowercasing;
/// ASCII is the floor. Applied identically to every comparison (collision,
/// disambiguation, lookup) — never compare a folded value against an unfolded one.
fn fold(name: &str) -> String {
    name.to_lowercase()
}
```

- [ ] **Step 2: Add the two `VirtualTree` fields**

Replace the `VirtualTree` struct (currently `tree.rs:77-83`) with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualTree {
    nodes: ImHashMap<u64, Node>,
    children: ImHashMap<u64, OrdMap<String, u64>>,
    rendered_children: ImHashMap<u64, OrdMap<String, OrdMap<String, u64>>>,
    /// `parent -> fold(name) -> inode`, populated ONLY when `case_insensitive`.
    /// Build-only (folding always full-rebuilds, so a published tree is never
    /// mutated). Gives O(1) folded lookup and collision checks. Part of structural
    /// equality; built deterministically, so fresh and re-rendered folded trees
    /// compare equal.
    folded_children: ImHashMap<u64, ImHashMap<String, u64>>,
    track_to_inode: ImHashMap<i64, u64>,
    /// When true, names are compared case-insensitively (dirs merge, files
    /// disambiguate). Defaults to false (exact matching, identical to Linux).
    case_insensitive: bool,
}
```

- [ ] **Step 3: Add `build_with_ci` and make `build_with` delegate**

Replace `build` and `build_with` (currently `tree.rs:88-116`) with:

```rust
    pub fn build(entries: &[(i64, String)]) -> VirtualTree {
        VirtualTree::build_with(entries, &mut InodeAllocator::new())
    }

    /// Case-sensitive build (the default everywhere except a folded mount).
    /// Inodes are assigned via `alloc` (keyed by rendered path), stable across
    /// rebuilds that reuse the same allocator.
    pub fn build_with(entries: &[(i64, String)], alloc: &mut InodeAllocator) -> VirtualTree {
        VirtualTree::build_with_ci(entries, alloc, false)
    }

    /// Build with explicit case-folding. With `case_insensitive`, names fold:
    /// case-variant directories merge, case-variant files disambiguate.
    pub fn build_with_ci(
        entries: &[(i64, String)],
        alloc: &mut InodeAllocator,
        case_insensitive: bool,
    ) -> VirtualTree {
        let mut tree = VirtualTree {
            nodes: ImHashMap::new(),
            children: ImHashMap::new(),
            rendered_children: ImHashMap::new(),
            folded_children: ImHashMap::new(),
            track_to_inode: ImHashMap::new(),
            case_insensitive,
        };
        tree.nodes.insert(
            Self::ROOT,
            Node {
                parent: Self::ROOT,
                name: String::new(),
                rendered_name: String::new(),
                kind: NodeKind::Dir,
            },
        );
        tree.children.insert(Self::ROOT, OrdMap::new());
        tree.rendered_children.insert(Self::ROOT, OrdMap::new());
        for (track_id, path) in entries {
            tree.insert_file(*track_id, path, alloc);
        }
        tree
    }
```

(An empty dir gets no `folded_children` bucket until it has a child — matching how
`rendered_children` only creates buckets on insert.)

- [ ] **Step 4: Add the folded-index maintenance + query helpers**

In `musefs-core/src/tree.rs`, add these methods inside `impl VirtualTree`, next to
`insert_rendered_child` (~line 324):

```rust
    /// Mirror a child insertion into the folded index (no-op unless folding).
    fn insert_folded_child(&mut self, parent: u64, name: &str, inode: u64) {
        if !self.case_insensitive {
            return;
        }
        self.folded_children
            .entry(parent)
            .or_default()
            .insert(fold(name), inode);
    }

    /// True if `name` is already taken in `dir` (folded when case-insensitive).
    fn taken(&self, dir: u64, name: &str) -> bool {
        if self.case_insensitive {
            self.folded_children
                .get(&dir)
                .is_some_and(|b| b.contains_key(&fold(name)))
        } else {
            self.children.get(&dir).is_some_and(|c| c.contains_key(name))
        }
    }

    /// An existing *directory* child of `dir` matching `name` (folded when
    /// case-insensitive), for merge reuse. `None` if absent or a non-dir.
    fn dir_child_named(&self, dir: u64, name: &str) -> Option<u64> {
        let ino = if self.case_insensitive {
            self.folded_children
                .get(&dir)
                .and_then(|b| b.get(&fold(name)).copied())
        } else {
            self.children.get(&dir).and_then(|c| c.get(name).copied())
        }?;
        self.is_dir(ino).then_some(ino)
    }
```

- [ ] **Step 5: Maintain the folded index at the two insert sites**

In `insert_file` (currently ends at `tree.rs:191` with `insert_rendered_child`),
add the folded mirror immediately after:

```rust
        self.insert_rendered_child(dir, raw_name, &name, inode);
        self.insert_folded_child(dir, &name, inode);
    }
```

In `ensure_dir`'s creation branch (currently ends at `tree.rs:224` with
`insert_rendered_child`), add the folded mirror immediately after:

```rust
        self.insert_rendered_child(parent, name, &unique, inode);
        self.insert_folded_child(parent, &unique, inode);
        (inode, full)
    }
```

- [ ] **Step 6: Fold `disambiguate`**

Replace `disambiguate` (currently `tree.rs:229-241`) with the `taken`-based form:

```rust
    /// Return `name` if free in `dir`, else append ` (k)` before the extension.
    /// Freeness is case-folded when the tree is case-insensitive.
    fn disambiguate(&self, dir: u64, name: &str) -> String {
        if !self.taken(dir, name) {
            return name.to_string();
        }
        for k in 2u32.. {
            let candidate = suffix_candidate(name, k);
            if !self.taken(dir, &candidate) {
                return candidate;
            }
        }
        unreachable!("an unoccupied candidate rank always exists")
    }
```

(In case-sensitive mode `taken` delegates to the exact `children` check — identical
to the original `existing.contains_key`.)

- [ ] **Step 7: Fold the merge check in `ensure_dir`**

Replace the reuse check at the top of `ensure_dir` (currently `tree.rs:201-205`)
with `dir_child_named`, using the *existing* dir's stored name for the path (its
case may differ from the queried `name` under folding):

```rust
        if let Some(existing) = self.dir_child_named(parent, name) {
            let stored = self.node(existing).expect("dir_child_named node").name.clone();
            return (existing, join_path(parent_path, &stored));
        }
```

(In case-sensitive mode `dir_child_named` matches the exact key, so `stored == name`
and behavior is unchanged.)

- [ ] **Step 8: Fold `lookup`**

Replace `lookup` (currently `tree.rs:139-143`) with:

```rust
    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        if self.case_insensitive {
            self.folded_children
                .get(&parent)
                .and_then(|b| b.get(&fold(name)).copied())
        } else {
            self.children
                .get(&parent)
                .and_then(|c| c.get(name).copied())
        }
    }
```

- [ ] **Step 9: Add `case_insensitive` to `MountConfig`**

In `musefs-core/src/facade.rs`, extend the `MountConfig` struct (currently
`facade.rs:34-42`):

```rust
pub struct MountConfig {
    pub template: String,
    pub fallbacks: BTreeMap<String, String>,
    pub default_fallback: String,
    pub mode: Mode,
    /// Minimum time between `data_version` polls; a metadata-op storm within this
    /// window skips the poll entirely. `Duration::ZERO` disables debouncing.
    pub poll_interval: std::time::Duration,
    /// Compare filenames case-insensitively (dirs merge, files disambiguate).
    /// Set by the CLI (`--case-insensitive`), default true on macOS.
    pub case_insensitive: bool,
}
```

- [ ] **Step 10: Bypass `rebuild_incremental` when folding**

In `musefs-core/src/facade.rs`, add this as the first statement of
`rebuild_incremental` (currently `facade.rs:407`, right after the signature):

```rust
        // Case-insensitive trees use full rebuilds: the incremental path
        // navigates by exact rendered name, which a folded (merged) tree can
        // mismatch. `Ok(None)` routes the caller to `rebuild_full`, which builds a
        // correct folded tree via `build_with_ci`. (The O(changed) optimization
        // stays case-sensitive-only.)
        if self.config.case_insensitive {
            return Ok(None);
        }
```

NOTE: the existing `Ok(None)` handler (`facade.rs:665`) logs "changelog gap;
falling back to full refresh" and bumps `gap_fallbacks`. That wording/counter is
cosmetically off for the case-insensitive case but functionally correct (it does
a full rebuild). Leaving it is acceptable for this best-effort path.

- [ ] **Step 11: Move the 5 production `build_with` calls to `build_with_ci`**

In `musefs-core/src/facade.rs`, change each production call to pass the flag:

- `Musefs::open` (~line 231) — `config` is the param in scope:
  ```rust
  let tree = VirtualTree::build_with_ci(&entries, &mut alloc, config.case_insensitive);
  ```
- `build_full` (~line 324) — `self.config` in scope:
  ```rust
  Ok((VirtualTree::build_with_ci(&entries, alloc, self.config.case_insensitive), snapshot))
  ```
- `rebuild_full` (~line 359):
  ```rust
  let tree = VirtualTree::build_with_ci(&entries, &mut alloc, self.config.case_insensitive);
  ```
- the incremental reference equiv check (~line 507) — bypassed when folding, but
  keep it building like-for-like:
  ```rust
  let reference = VirtualTree::build_with_ci(&entries, &mut ref_alloc, self.config.case_insensitive);
  ```
- the incremental fallback (~line 522):
  ```rust
  VirtualTree::build_with_ci(&entries, &mut alloc, self.config.case_insensitive)
  ```

NOTE: if `build_full` does not have `&self` in scope (verify), thread the flag in
from its caller instead of `self.config`. The other four are `&self` methods.

- [ ] **Step 12: Add `case_insensitive: false` to every `MountConfig` literal**

Run: `cargo build --workspace --all-targets 2>&1 | grep -A2 "missing field"`
Expected: a list of every `MountConfig { ... }` literal missing the new field.

Add `case_insensitive: false,` to each (the build output is authoritative). Known
sites:
- `musefs-cli/src/lib.rs:144` (`parse_mount_config` — keep `false` for now; Task 3 wires the real flag)
- `musefs-core/src/facade.rs` tests: ~1153, ~1244, ~1334, ~1372, ~1418, ~1484
- `musefs-core/benches/read_throughput.rs:13`
- `musefs-core/tests/`: `incremental_refresh.rs:19`, `facade.rs:8` (+ `457/510/566/624/683`), `bench_refresh.rs:12`, `bench_ingest.rs:187`, `metrics.rs:18`, `binary_tag_tree.rs:7`, `flac_binary_tags.rs:46`
- `musefs-fuse/src/lib.rs:640` (`test_fs` helper)
- `musefs-fuse/tests/`: `concurrency.rs:60`, `ogg_read_through.rs:59`, `playback_pcm.rs:19`, `passthrough.rs:61`, `keep_cache.rs:52`, `mount.rs:49`

- [ ] **Step 13: Build, lint, full suite**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: all green (everything constructs `case_insensitive: false`, so behavior
is byte-identical to before this task).

- [ ] **Step 14: Confirm the fuzz crate still builds**

The fuzz crate binds to `musefs-format`, not `musefs-core`, so it is unaffected.
Confirm if nightly is available:
Run: `cargo +nightly fuzz build 2>/dev/null || echo "skipped (no nightly/cargo-fuzz)"`
Expected: builds, or a clean skip.

- [ ] **Step 15: Commit**

```bash
git add musefs-core/src/tree.rs musefs-core/src/facade.rs musefs-cli/src/lib.rs \
        musefs-core/benches musefs-core/tests musefs-fuse/src/lib.rs musefs-fuse/tests
git commit -m "feat(core): case-folding build path + folding bypasses incremental refresh"
```

---

## Task 2: Case-insensitive behavior + refresh acceptance tests

**Files:**
- Modify: `musefs-core/src/tree.rs` (new `#[cfg(test)]` cases)
- Modify: `musefs-core/tests/incremental_refresh.rs` (the refresh acceptance test)

- [ ] **Step 1: Unit tests for merge / disambiguate / lookup**

In `musefs-core/src/tree.rs`, inside the existing `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn case_insensitive_merges_directories() {
        // Two artist dirs differing only by case collapse into one; both titles
        // live under the first-seen casing.
        let entries = vec![
            (1i64, "Foo/A".to_string()),
            (2i64, "foo/B".to_string()),
        ];
        let tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(), true);
        let foo = tree.lookup(VirtualTree::ROOT, "Foo").expect("Foo dir");
        // Case-insensitive lookup resolves any casing to the same inode.
        assert_eq!(tree.lookup(VirtualTree::ROOT, "foo"), Some(foo));
        assert_eq!(tree.lookup(VirtualTree::ROOT, "FOO"), Some(foo));
        // Exactly one child of root (the merged dir), with both files under it.
        assert_eq!(tree.children(VirtualTree::ROOT).unwrap().len(), 1);
        assert!(tree.lookup(foo, "A").is_some());
        assert!(tree.lookup(foo, "B").is_some());
        assert_eq!(tree.children(foo).unwrap().len(), 2);
    }

    #[test]
    fn case_insensitive_disambiguates_leaf_files() {
        // Two files in the same dir whose names differ only by case must NOT
        // collide: the second is disambiguated.
        let entries = vec![
            (1i64, "Dir/Song".to_string()),
            (2i64, "Dir/song".to_string()),
        ];
        let tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(), true);
        let dir = tree.lookup(VirtualTree::ROOT, "Dir").expect("Dir");
        let names: Vec<String> = tree.children(dir).unwrap().keys().cloned().collect();
        // First-seen "Song" keeps its name; "song" becomes "song (2)".
        assert!(names.contains(&"Song".to_string()));
        assert!(names.contains(&"song (2)".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn case_sensitive_keeps_both_dirs_separate() {
        // Control: with folding OFF, "Foo" and "foo" are distinct dirs and a
        // differently-cased query misses.
        let entries = vec![
            (1i64, "Foo/A".to_string()),
            (2i64, "foo/B".to_string()),
        ];
        let tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(), false);
        assert_eq!(tree.children(VirtualTree::ROOT).unwrap().len(), 2);
        assert_ne!(
            tree.lookup(VirtualTree::ROOT, "Foo"),
            tree.lookup(VirtualTree::ROOT, "foo")
        );
        assert_eq!(tree.lookup(VirtualTree::ROOT, "FOO"), None);
    }
```

- [ ] **Step 2: Run them**

Run: `cargo test -p musefs-core --lib tree::tests::case_`
Expected: PASS — all three.

- [ ] **Step 3: Add the case-insensitive refresh acceptance test**

This is the acceptance gate: it proves (a) a folded tree merges case-variant
artists through the public facade, and (b) an external edit is still reflected
after `poll_refresh` (i.e. the incremental bypass correctly routes to a full
folded rebuild that matches a fresh folded build).

In `musefs-core/tests/incremental_refresh.rs`, add a folded config helper next to
`config` (line 18) and the test:

```rust
fn config_ci() -> MountConfig {
    MountConfig {
        case_insensitive: true,
        ..config()
    }
}

#[test]
fn case_insensitive_refresh_merges_and_matches_full_rebuild() {
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();

    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();

    // Make the two tracks' artists differ only by case (same album so they share
    // a parent): under folding the artist dir must MERGE.
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();
    writer
        .replace_tags(
            ids[0],
            &[Tag::new("ARTIST", "Foo", 0), Tag::new("ALBUM", "Al", 0), Tag::new("TITLE", "One", 0)],
        )
        .unwrap();
    writer
        .replace_tags(
            ids[1],
            &[Tag::new("ARTIST", "foo", 0), Tag::new("ALBUM", "Al", 0), Tag::new("TITLE", "Two", 0)],
        )
        .unwrap();

    let fs = Musefs::open(Db::open(&db_path).unwrap(), config_ci()).unwrap();

    // Exactly one top-level artist directory (the merged "Foo"/"foo").
    let fp = tree_fingerprint(&fs);
    let top_dirs: std::collections::BTreeSet<String> = fp
        .keys()
        .map(|p| p.split('/').next().unwrap().to_string())
        .collect();
    assert_eq!(top_dirs.len(), 1, "case-variant artists must merge into one dir");

    // An external edit is still picked up — incremental is bypassed, so this goes
    // through a full folded rebuild — and the result matches a fresh folded build.
    writer
        .replace_tags(
            ids[1],
            &[Tag::new("ARTIST", "foo", 0), Tag::new("ALBUM", "Al", 0), Tag::new("TITLE", "Renamed", 0)],
        )
        .unwrap();
    fs.poll_refresh().unwrap();

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config_ci()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
        "case-insensitive refresh (full rebuild) must match a fresh folded build"
    );
}
```

NOTE: confirm `small_corpus(2)` yields ≥2 tracks and `Tag::new`/`replace_tags`/
`list_tracks` signatures match the existing test above it (mirror them exactly).

- [ ] **Step 4: Run the acceptance test**

Run: `cargo test -p musefs-core --test incremental_refresh case_insensitive_refresh`
Expected: PASS.

- [ ] **Step 5: Full suite + commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: green.

```bash
git add musefs-core/src/tree.rs musefs-core/tests/incremental_refresh.rs
git commit -m "test(core): case-insensitive merge/disambiguate/lookup + folded refresh"
```

---

## Task 3: Wire the `--case-insensitive` CLI flag (OS-defaulted)

**Files:**
- Modify: `musefs-cli/src/lib.rs`

- [ ] **Step 1: Write the failing parse tests**

In `musefs-cli/src/lib.rs`, inside `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn case_insensitive_defaults_to_os() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, _) = parse_mount_config(&args);
        assert_eq!(config.case_insensitive, cfg!(target_os = "macos"));
    }

    #[test]
    fn case_insensitive_is_overridable() {
        use clap::Parser;
        for (val, want) in [("true", true), ("false", false)] {
            let cli = Cli::try_parse_from([
                "musefs", "mount", "/mnt", "--db", "/tmp/x.db", "--case-insensitive", val,
            ])
            .unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("expected Mount");
            };
            assert_eq!(args.case_insensitive, want);
        }
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p musefs-cli case_insensitive`
Expected: FAIL — `no field case_insensitive on MountArgs` (compile error).

- [ ] **Step 3: Add the flag to `MountArgs`**

In `musefs-cli/src/lib.rs`, add to the `MountArgs` struct (after `keep_cache`,
~line 76):

```rust
    /// Compare filenames case-insensitively: case-variant directories merge and
    /// case-variant files are disambiguated. Defaults to true on macOS (whose
    /// volumes are usually case-insensitive), false on Linux/FreeBSD. Override
    /// with `--case-insensitive false` (e.g. a case-sensitive APFS volume).
    #[arg(long, default_value_t = cfg!(target_os = "macos"), action = clap::ArgAction::Set)]
    pub case_insensitive: bool,
```

- [ ] **Step 4: Set the field in `parse_mount_config`**

In `parse_mount_config` (currently `musefs-cli/src/lib.rs:143-158`), replace the
`case_insensitive: false,` placeholder (added in Task 1 Step 12) with the real flag:

```rust
        mode: args.mode.into(),
        poll_interval: std::time::Duration::from_millis(args.poll_interval_ms),
        case_insensitive: args.case_insensitive,
    };
```

- [ ] **Step 5: Run the parse tests**

Run: `cargo test -p musefs-cli case_insensitive`
Expected: PASS — both tests.

- [ ] **Step 6: Full suite + commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: green.

```bash
git add musefs-cli/src/lib.rs
git commit -m "feat(cli): --case-insensitive flag (default true on macOS)"
```

---

## Task 4: Document the flag

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Find where mount flags are documented**

Run: `grep -rn "keep-cache\|poll-interval\|--mode\|max-readahead" README.md | head`
Expected: locate the `mount` flags reference (CLAUDE.md's doc map points usage/CLI
flags to `README.md`).

- [ ] **Step 2: Document `--case-insensitive`**

Add an entry alongside the other `mount` flags:

```markdown
- `--case-insensitive <true|false>` — compare filenames case-insensitively.
  Case-variant directories merge into one (first-seen casing wins) and
  case-variant files are disambiguated with a ` (2)` suffix. Defaults to `true`
  on macOS (whose volumes are usually case-insensitive) and `false` on
  Linux/FreeBSD; override for a case-sensitive APFS volume. (Case-insensitive
  mounts refresh via a full rebuild rather than the incremental fast path.)
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document --case-insensitive mount flag"
```

---

## Final verification

- [ ] **Step 1: Full workspace gate (mirrors pre-commit)**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 2: Confirm zero overhead when folding is off**

Run: `grep -n "folded_children" musefs-core/src/tree.rs`
Expected: the only mutation is in `insert_folded_child`, guarded by
`if !self.case_insensitive { return; }`. The case-sensitive path never populates
the index.

- [ ] **Step 3: Confirm the incremental path is untouched by folding**

Run: `grep -n "fold(" musefs-core/src/tree.rs`
Expected: `fold(` appears only in `taken`, `dir_child_named`, `lookup`, and
`insert_folded_child` — NOT in `collision_gate`, `deepest_existing_ancestor`,
`children_by_rendered`, `apply_changes`, `remove_track`, or
`prune_empty_dirs_upward` (those serve the incremental path, which folding bypasses).

- [ ] **Step 4: Confirm the acceptance test passes**

Run: `cargo test -p musefs-core --test incremental_refresh case_insensitive_refresh_merges_and_matches_full_rebuild`
Expected: PASS.
