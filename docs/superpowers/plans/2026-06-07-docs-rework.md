# Docs Rework Implementation Plan (issue #64)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the scattered, partially stale documentation with the coherent set designed in `docs/superpowers/specs/2026-06-07-docs-rework-design.md`: usage-first README, ARCHITECTURE.md, CONTRIBUTING.md, SECURITY.md, five per-format docs under `docs/`, a lean CLAUDE.md with AGENTS.md as a symlink.

**Architecture:** Pure documentation work, greenfield method: every document is written from the **current code** (each factual claim verified against the listed source files as it is written — never copied from an old doc), then a **back-validation pass** re-reads the old text it replaces and mines true, hard-won detail (each mined claim re-checked against code). Absorbed docs are deleted in the same task as their back-validation. A final sweep verifies links, the ROADMAP purge, the CLAUDE.md line budget, and the AGENTS.md symlink.

**Tech Stack:** Markdown only. No Rust/Python code changes. Verification via bash one-liners. Work happens in the worktree at `/home/cfutro/git/musefs/.claude/worktrees/docs-rework` (branch `worktree-docs-rework`) — run everything from that directory.

**Read the spec first:** `docs/superpowers/specs/2026-06-07-docs-rework-design.md`. It is the authority on each document's outline and on every decision below. This plan operationalizes it.

---

## Ground rules (apply to every task)

1. **Greenfield discipline.** When writing a doc, do NOT open the old doc it replaces until the draft is done. Write from the source files listed in the task. The old text is consulted only in the back-validation step.
2. **Per-claim verification.** Every behavioral claim (a flag name, a default, a limitation, an invariant) must be traceable to a source file/symbol or a command output you actually ran. If you can't verify it, don't write it.
3. **Back-validation.** After drafting, read the old text listed in the task. For each true-and-valuable fact missing from your draft, verify it against code, then fold it in. Stale claims are dropped silently — except in Task 10 (CLAUDE.md), where the **flag rule** applies: anything you think should *stay* in CLAUDE.md gets flagged to the user, never silently kept or dropped.
4. **Commits are slow.** The pre-commit hook runs fmt, clippy, the **full workspace test suite**, and ruff over `contrib/` + `tests/interop/`. Use a 600000 ms timeout on every `git commit`. Never use `--no-verify`.
5. **Tone.** Write plainly and concretely. No marketing fluff. Match the project's existing voice: dense, precise, willing to explain *why*.
6. **Out of scope.** `docs/superpowers/**` untouched. CHANGELOG.md and BENCHMARKS.md content untouched (link fixes only if a link breaks). No new ROADMAP.

## File map (end state)

| Path | Task | Action |
| ---- | ---- | ------ |
| `ARCHITECTURE.md` | 1 | Create |
| `docs/DB_CONTRACT.md` | 1 | Delete (absorbed) |
| `docs/FLAC.md` | 2 | Create |
| `docs/MP3.md` | 3 | Create |
| `docs/M4A.md` | 4 | Create |
| `docs/OGG.md` | 5 | Create |
| `docs/OGG_INVARIANT.md` | 5 | Delete (absorbed) |
| `docs/WAV.md` | 6 | Create |
| `CONTRIBUTING.md` | 7 | Create |
| `docs/COVERAGE.md` | 7 | Delete (absorbed) |
| `README.md` | 8 | Rewrite |
| `SECURITY.md` | 9 | Create |
| `CLAUDE.md` | 10 | Rewrite lean (< 100 lines) |
| `AGENTS.md` | 10 | Replace with symlink → `CLAUDE.md` |
| `contrib/{beets,picard,python-musefs}/README.md` | 11 | Light touch |
| (link check, ROADMAP purge, line/symlink checks) | 12 | Verify |

---

### Task 1: ARCHITECTURE.md

**Files:**
- Create: `ARCHITECTURE.md`
- Delete: `docs/DB_CONTRACT.md`

- [ ] **Step 1: Read the sources**

Read these (symbol-level reads are fine; you need the shapes, not every line):
- `Cargo.toml` — workspace members (note `musefs-latencyfs`; check its `Cargo.toml` for `publish = false`).
- `musefs-format/src/layout.rs` — `RegionLayout`, all five `Segment` variants.
- `musefs-core/src/reader.rs` — `read_at` walk, `HeaderCache::resolve`, `content_version` keying, `BackingChanged` size+mtime validation.
- `musefs-core/src/facade.rs` — `Mode`, `poll_refresh` / `poll_refresh_notify`, `data_version` stamping.
- `musefs-core/src/tree.rs` — `VirtualTree::build`, `disambiguate`, the persistent path→inode allocator.
- `musefs-core/src/template.rs` — `$field` / `${field}` syntax, fallbacks.
- `musefs-core/src/mapping.rs` — DB rows → `TagInput`/`ArtInput` and template fields.
- `musefs-core/src/scan.rs` — `scan_directory`, `revalidate` semantics.
- `musefs-db/src/schema.rs` — `MIGRATION_V1`, tables, triggers, `MIGRATIONS` append-only policy.
- `musefs-fuse/src/` — passthrough registration (kernel 6.9+, CAP_SYS_ADMIN gating, fallback), where `poll_refresh` is fired, `inval_inode` on `--keep-cache`.

- [ ] **Step 2: Write `ARCHITECTURE.md`**

Follow the spec's ARCHITECTURE outline exactly (§ "ARCHITECTURE.md — technical reference", 9 sections):
1. Design overview — cardinal invariant + one-paragraph serving model.
2. Crate layout — layered diagram (ASCII, like the old CLAUDE.md's), dependency direction, placement rules. Include `musefs-latencyfs` marked dev/bench-only (`publish = false`, BENCHMARKS.md harness).
3. Segment model — `RegionLayout`, the five variants, `read_at` splice walk. One paragraph per format + link `docs/<FMT>.md` (links will dangle until Tasks 2–6; final check is Task 12).
4. Mount modes — `Synthesis` vs `StructureOnly`, FUSE passthrough gating + fallback semantics.
5. SQLite store — schema shape, append-only migrations, **external-writer contract** (scanner-owned `tracks` columns vs writable `tags`/`art`/`track_art`; violated-contract behavior = controlled backing/layout error).
6. Freshness — `content_version` vs `data_version`, HeaderCache keying, debounced single-flighted refresh, `--keep-cache` invalidation.
7. Virtual tree — templates, disambiguation, stable inodes across rebuilds (vanished path → `ENOENT` bounded by TTL).
8. Scanning — ingest pipeline; `--revalidate` preserves external edits / prunes gone tracks / GCs orphaned art.
9. Contrib ecosystem (short) — python-musefs as store-contract library, beets/Picard as writers, generated `schema.py`. Link to `contrib/*/README.md`.

- [ ] **Step 3: Back-validate**

Read `docs/DB_CONTRACT.md` and the architecture sections of the **old** `CLAUDE.md` (git: `git show main:CLAUDE.md`). Mine missing true detail (e.g. DB_CONTRACT's per-column ownership list; CLAUDE.md's "commits the new stamp only after a successful rebuild", "at most one rebuild per interval"). Verify each mined claim against code before folding in.

- [ ] **Step 4: Delete the absorbed doc**

```bash
git rm docs/DB_CONTRACT.md
```

- [ ] **Step 5: Verify**

Run: `rg -n 'DB_CONTRACT' --glob '!docs/superpowers/**' .`
Expected: no hits outside this plan/spec (contrib hits, if any, are fixed in Task 11 — note them in your report).

- [ ] **Step 6: Commit**

```bash
git add ARCHITECTURE.md && git commit -m 'docs: add ARCHITECTURE.md, absorb DB_CONTRACT (#64)'
```
(600000 ms timeout; full test suite runs.)

---

### Task 2: docs/FLAC.md

**Files:**
- Create: `docs/FLAC.md`

- [ ] **Step 1: Read the sources**

- `musefs-format/src/flac.rs` — probe, `synthesize_layout`, which structural blocks are preserved (re-read from the file front) vs regenerated.
- `musefs-format/src/tagmap.rs` — canonical vocabulary ↔ Vorbis field mapping; extension-slot behavior; **binary-tag handling** (this is the area the old README predates — derive from code, not memory).
- `musefs-format/src/input.rs` — `TagInput` / `ArtInput` shapes.
- `musefs-core/src/mapping.rs` — multi-value semantics, casing preservation.
- `musefs-format/tests/proptest_flac.rs` and the FLAC rows in `musefs-core/tests/interop_emit.rs` — what is actually asserted to round-trip.

- [ ] **Step 2: Write `docs/FLAC.md`**

Use the spec's shared 5-section shape (§ "docs/{FLAC,…}.md"): scope line; what round-trips; lossy edges (**derived from the code/tests read in Step 1 — the old README list is off-limits here**); how synthesis works (segment-by-segment layout, preserved structural blocks); quirks & invariants.

- [ ] **Step 3: Back-validate**

Read the old README's "Tag handling" section (`git show main:README.md`) and any FLAC-specific prose in old CLAUDE.md. Mine FLAC-relevant true claims only; verify each against `flac.rs`/`tagmap.rs` before folding in. Claims contradicted by current code (e.g. anything predating binary-tag support) are dropped.

- [ ] **Step 4: Commit**

```bash
git add docs/FLAC.md && git commit -m 'docs: add per-format doc for FLAC (#64)'
```

---

### Task 3: docs/MP3.md

**Files:**
- Create: `docs/MP3.md`

- [ ] **Step 1: Read the sources**

- `musefs-format/src/mp3.rs` — ID3v2 regeneration, version normalization, frame mapping, binary-frame handling, the Xing/LAME info frame traveling with audio, `COMM`/`USLT` handling.
- `musefs-format/src/tagmap.rs` — canonical vocabulary ↔ ID3 frame mapping, `TXXX` extension slot, unmapped-standard-frame round-trip.
- `musefs-format/tests/proptest_mp3.rs` and MP3 rows in `musefs-core/tests/interop_emit.rs`.

- [ ] **Step 2: Write `docs/MP3.md`** — same 5-section shape. Lossy edges derived from Step 1 sources only.

- [ ] **Step 3: Back-validate** — old README "Tag handling" MP3/ID3 bullets + old CLAUDE.md MP3 prose, per-claim code check, fold or drop.

- [ ] **Step 4: Commit**

```bash
git add docs/MP3.md && git commit -m 'docs: add per-format doc for MP3 (#64)'
```

---

### Task 4: docs/M4A.md

**Files:**
- Create: `docs/M4A.md`

- [ ] **Step 1: Read the sources**

- `musefs-format/src/mp4.rs` — `moov` rebuild, `stco`/`co64` patching, atom mapping, `----` freeform handling (`mean` normalization, multi-value behavior), binary atoms (`trkn`/`disk` and beyond), M4B coverage.
- `musefs-format/src/tagmap.rs` — canonical vocabulary ↔ MP4 atom mapping.
- `musefs-format/tests/proptest_mp4.rs` and MP4 rows in `musefs-core/tests/interop_emit.rs`.

- [ ] **Step 2: Write `docs/M4A.md`** — same 5-section shape; scope line covers M4A/M4B. Lossy edges from Step 1 sources only.

- [ ] **Step 3: Back-validate** — old README MP4 bullets + old CLAUDE.md M4A prose, per-claim code check, fold or drop.

- [ ] **Step 4: Commit**

```bash
git add docs/M4A.md && git commit -m 'docs: add per-format doc for M4A (#64)'
```

---

### Task 5: docs/OGG.md

**Files:**
- Create: `docs/OGG.md`
- Delete: `docs/OGG_INVARIANT.md`

- [ ] **Step 1: Read the sources**

- `musefs-format/src/ogg/` (whole module) — page renumbering (`seq_delta`), in-place CRC recompute, header regeneration, multiplexed/chained detection-and-skip, the `OggAudio` and `OggArtSlice` segment use.
- `musefs-format/src/vorbiscomment.rs` — VorbisComment rebuild.
- `musefs-core/src/ogg_index.rs` — rendered-lookup indexing (mention only if it surfaces user/dev-visible behavior).
- Art split: base64 `METADATA_BLOCK_PICTURE` (Opus/Vorbis) vs native FLAC PICTURE (FLAC-in-Ogg) — find both paths in the ogg module.
- `musefs-format/tests/proptest_ogg.rs` and Ogg rows in `musefs-core/tests/interop_emit.rs`.

- [ ] **Step 2: Write `docs/OGG.md`** — same 5-section shape. Scope line: Opus, Vorbis, FLAC-in-Ogg; multiplexed/chained detected and skipped. Section 4 states the invariant absorbed from OGG_INVARIANT.md (packet payload bytes preserved; page seq numbers + CRCs intentionally patched) **and** its "verified by" list (proptest_ogg, read_at payload-comparison tests, mutagen interop), re-verified to still exist. Covers the art split explicitly.

- [ ] **Step 3: Back-validate** — `docs/OGG_INVARIANT.md` (its content should now be fully represented or consciously dropped), old README Ogg bullets, old CLAUDE.md Ogg prose. Per-claim code check.

- [ ] **Step 4: Delete the absorbed doc**

```bash
git rm docs/OGG_INVARIANT.md
```

- [ ] **Step 5: Verify**

Run: `rg -n 'OGG_INVARIANT' --glob '!docs/superpowers/**' .`
Expected: no hits (note any contrib hits for Task 11).

- [ ] **Step 6: Commit**

```bash
git add docs/OGG.md && git commit -m 'docs: add per-format doc for Ogg, absorb OGG_INVARIANT (#64)'
```

---

### Task 6: docs/WAV.md

**Files:**
- Create: `docs/WAV.md`

- [ ] **Step 1: Read the sources**

- `musefs-format/src/wav.rs` — RIFF front regeneration (`LIST`/`INFO` chunk + embedded `id3 ` chunk), verbatim `data` payload, ID3-in-WAV specifics.
- `musefs-format/src/tagmap.rs` — which fields land in `INFO` vs the `id3 ` chunk.
- `musefs-format/tests/proptest_wav.rs` and WAV rows in `musefs-core/tests/interop_emit.rs`.

- [ ] **Step 2: Write `docs/WAV.md`** — same 5-section shape. Lossy edges from Step 1 sources only.

- [ ] **Step 3: Back-validate** — old README WAV bullets + old CLAUDE.md WAV prose, per-claim code check, fold or drop.

- [ ] **Step 4: Commit**

```bash
git add docs/WAV.md && git commit -m 'docs: add per-format doc for WAV (#64)'
```

---

### Task 7: CONTRIBUTING.md

**Files:**
- Create: `CONTRIBUTING.md`
- Delete: `docs/COVERAGE.md`

- [ ] **Step 1: Read the sources**

- `.githooks/pre-commit` — the **actual** hook steps (fmt, clippy, full workspace tests, ruff over `contrib/` + `tests/interop/`). Quote what it really runs.
- `scripts/mutants.sh` — the mutation-gate wrapper; document invocation, not internals.
- `.cargo/mutants.toml` — exclude policy worth one sentence.
- `Cargo.toml` — `[workspace.lints]` location, members.
- `fuzz/` — target list (`ls fuzz/fuzz_targets/`), `generate_seeds`, out-of-workspace status.
- `tests/interop/` — requirements + invocation.
- `.github/workflows/` — `ci.yml`, `coverage.yml`, `mutants.yml`, `fuzz.yml`, `audit.yml`, `release.yml`: name the required aggregator checks (`ci-ok`, `coverage-ok`) and what the fuzz smoke job covers.
- `contrib/python-musefs/`, `contrib/beets/`, `contrib/picard/` — test invocations (venv for beets, system Python + PYTHONPATH for real-Picard/Qt tests), `vendor_to_picard.py`, `constants.py` (`MAX_ART_BYTES`), the `MUSEFS_REGEN_SCHEMA_PY=1` regeneration test.
- `docs/COVERAGE.md` content is absorbed in Step 3.

- [ ] **Step 2: Write `CONTRIBUTING.md`**

Follow the spec's CONTRIBUTING outline exactly (8 sections):
1. Getting set up — toolchain, FUSE prereqs, `git config core.hooksPath .githooks`, full hook surface including ruff.
2. Build & test — command reference incl. FUSE e2e (`--ignored`, `/dev/fuse`; passthrough e2e via sudo on the prebuilt test binary).
3. Test tiers — property tests (`fuzzing` feature); fuzzing (nightly, out-of-workspace caveat, `cargo +nightly fuzz build` locally); mutagen interop; mutation gate referencing `scripts/mutants.sh`, with the TMPDIR nuance (tmpfs fine/preferable for small in-diff sets; cgroup + on-disk TMPDIR for large sets — allocation-bomb mutants can OOM the host; sharding exists, workflow not built out), the empty-diff false-pass warning, the no-pipe exit-code warning.
4. Coverage — cargo-llvm-cov locally, Codecov in CI, why musefs-fuse is excluded.
5. Code conventions — error idioms, integer-conversion convention, lint policy location, layer-placement rule, benches//tests/ API consumers → `clippy --all-targets`.
6. Adding a format — probe + `synthesize_layout`, `Format` enum, reader/scan wiring, full test surface (fixture, fuzz target + seed, proptest, interop manifest row), **and** "write `docs/<FMT>.md`".
7. Python plugins — three test suites with their gotchas; hand-mirrored `MAX_ART_BYTES`; `schema.py` regeneration + re-vendor.
8. PRs & commits — conventional subjects, scoped commits, required CI checks, benchmarks → BENCHMARKS.md.

- [ ] **Step 3: Back-validate**

Read `docs/COVERAGE.md`, old CLAUDE.md (commands + conventions + contrib sections), old README "Development"/"Fuzzing" sections, and `AGENTS.md`. Mine and code-verify. COVERAGE.md's content must be fully represented or consciously dropped.

- [ ] **Step 4: Delete the absorbed doc**

```bash
git rm docs/COVERAGE.md
```

- [ ] **Step 5: Verify**

Run: `rg -n 'docs/COVERAGE' --glob '!docs/superpowers/**' .`
Expected: no hits (note any for Task 11).

- [ ] **Step 6: Commit**

```bash
git add CONTRIBUTING.md && git commit -m 'docs: add CONTRIBUTING.md, absorb COVERAGE (#64)'
```

---

### Task 8: README.md rewrite

**Files:**
- Modify: `README.md` (full rewrite)

- [ ] **Step 1: Read the sources**

- `musefs-cli/src/` — clap definitions for `scan` and `mount`: every flag, every default. Cross-check by building and running help: `cargo run -p musefs -- mount --help` and `... scan --help`.
- `musefs-core/src/template.rs` — template syntax for the usage section.
- `Cargo.toml` of the `musefs` crate — published name, for install instructions.
- `CHANGELOG.md` — current release status for the Status section.
- Tasks 1–7 outputs — link targets (`ARCHITECTURE.md`, `CONTRIBUTING.md`, `docs/<FMT>.md`, `contrib/*/README.md`).

- [ ] **Step 2: Write the new `README.md`**

Follow the spec's README outline exactly (7 sections): hook + CI badge; quick start; what it's for; usage (scan/revalidate/mount, template syntax, two modes one sentence each, tuning-flags table — verify every row against clap defaults); supported-formats table linking `docs/<FMT>.md` with a 2–3 line tag-handling summary; FAQ (the six spec-listed questions, including write mounts as a **permanent non-goal**); Requirements / Status / License — Status has **no deferral list**.

- [ ] **Step 3: Back-validate**

Read `git show main:README.md`. Mine anything true and user-valuable the new draft lacks (e.g. multiplexed/chained Ogg skip note, `git config core.hooksPath` pointer — though hook detail now belongs to CONTRIBUTING). Per-claim code check. The dead `docs/ROADMAP.md` link must NOT survive.

- [ ] **Step 4: Verify**

Run: `rg -n 'ROADMAP' README.md`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add README.md && git commit -m 'docs: rewrite README usage-first (#64)'
```

---

### Task 9: SECURITY.md

**Files:**
- Create: `SECURITY.md`

- [ ] **Step 1: Write `SECURITY.md`**

Per the spec's SECURITY outline: supported versions (latest release — check CHANGELOG.md for the current version); private reporting via GitHub's advisories page (`https://github.com/Sohex/musefs/security/advisories/new`); what to expect (acknowledgment, fix, credit); one musefs-specific paragraph (threat surface = parsing untrusted media at scan time + serving at read time; fuzz/property suites target it; parser DoS findings are in-scope — cite the CHANGELOG's fixed parser-DoS entries as precedent). Keep it short (~30 lines). Enabling the repo setting is a follow-up outside this PR — note it in the task report, not the doc.

- [ ] **Step 2: Commit**

```bash
git add SECURITY.md && git commit -m 'docs: add SECURITY.md (#64)'
```

---

### Task 10: CLAUDE.md lean rewrite + AGENTS.md symlink — ⚠ USER CHECKPOINT

**Files:**
- Modify: `CLAUDE.md` (full rewrite, < 100 lines)
- Delete + recreate: `AGENTS.md` (symlink)

- [ ] **Step 1: Write the new `CLAUDE.md`**

Greenfield per the spec's 5-section filter ("declarative facts the agent needs on every task, nothing with a better home elsewhere"):
1. What this is — read-only passthrough FUSE fs; cardinal invariant (never relax it); SQLite store as source of truth.
2. Style — "musefs is written in clean, performant, idiomatic Rust" + layer-placement rule.
3. Everyday commands — build / test (workspace, per-crate, substring) / `cargo clippy --all-targets` / `cargo fmt`.
4. Pointers table — ARCHITECTURE.md, CONTRIBUTING.md, `docs/<FMT>.md`, store contract (ARCHITECTURE §5), one line each.
5. Repo-operational facts — pre-commit runs the full suite (red-test commits rejected); FUSE e2e is `--ignored`; contrib Python gotchas (one line + CONTRIBUTING link); schema change → regenerate `schema.py` (one line + CONTRIBUTING link).

Hard budget: `wc -l CLAUDE.md` < 100.

- [ ] **Step 2: Back-validate with the flag rule — STOP for user decision**

Read `git show main:CLAUDE.md` section by section. Classify every piece of content: (a) covered by the new CLAUDE.md, (b) has a home in ARCHITECTURE/CONTRIBUTING/format docs (verify it actually landed there — name the section), (c) stale/dropped, or (d) **keep-candidate**: you believe it should stay in CLAUDE.md itself. Compile the (d) list with one-line justifications **and present it to the user for a decision before finalizing**. Do not silently keep or drop anything in category (d). Also report any category-(b) item that did NOT actually land in its home — that's a gap to fix before committing.

- [ ] **Step 3: Apply the user's decisions** to CLAUDE.md; re-check `wc -l CLAUDE.md` < 100.

- [ ] **Step 4: Replace AGENTS.md with the symlink**

```bash
git rm AGENTS.md
ln -s CLAUDE.md AGENTS.md
git add AGENTS.md
```

- [ ] **Step 5: Verify**

Run: `readlink AGENTS.md && test -e AGENTS.md && echo OK && wc -l < CLAUDE.md`
Expected: `CLAUDE.md`, `OK`, a number < 100.
Also: `git ls-files -s AGENTS.md` — mode must be `120000` (symlink).

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md && git commit -m 'docs: lean CLAUDE.md, AGENTS.md as symlink (#64)'
```

---

### Task 11: contrib READMEs light touch

**Files:**
- Modify (as needed): `contrib/beets/README.md`, `contrib/picard/README.md`, `contrib/python-musefs/README.md`

- [ ] **Step 1: Find invalidated references**

```bash
rg -n 'DB_CONTRACT|OGG_INVARIANT|COVERAGE\.md|ROADMAP|CLAUDE\.md|\.\./\.\./README' contrib/*/README.md
```
Also re-read all three READMEs end to end for claims invalidated by the rework (e.g. "see the README's Tag handling section" → now `docs/<FMT>.md`; store-contract pointers → ARCHITECTURE §5).

- [ ] **Step 2: Apply minimal fixes** — re-point links, reword sentences that name moved/deleted docs. No substantive rewrites (spec: light touch only).

- [ ] **Step 3: Commit** (skip if Step 1 found nothing and Step 2 changed nothing — report that instead)

```bash
git add contrib/beets/README.md contrib/picard/README.md contrib/python-musefs/README.md
git commit -m 'docs: re-point contrib README references after rework (#64)'
```

---

### Task 12: Final verification sweep

**Files:** none (verification only; fix-ups allowed)

- [ ] **Step 1: Repo-wide markdown link check**

Run this from the worktree root (checks every relative link in tracked markdown outside `docs/superpowers/`):

```bash
fail=0
while IFS=: read -r f link; do
  target="${link%%#*}"; [ -z "$target" ] && continue
  case "$target" in http*|mailto:*) continue;; esac
  if ! [ -e "$(dirname "$f")/$target" ]; then echo "DEAD: $f -> $link"; fail=1; fi
done < <(git ls-files '*.md' | grep -v '^docs/superpowers/' \
  | xargs -I{} grep -oHE '\]\(([^)]+)\)' {} | sed -E 's/\]\(([^)]+)\)/\1/')
[ "$fail" -eq 0 ] && echo LINKS-OK
```
Expected: `LINKS-OK`. Fix any `DEAD:` lines (in the doc owning the link) and re-run.

- [ ] **Step 2: ROADMAP purge check**

Run: `git grep -n 'ROADMAP' -- ':!docs/superpowers'`
Expected: no output.

- [ ] **Step 3: Absorbed-docs accounting**

Confirm `docs/DB_CONTRACT.md`, `docs/OGG_INVARIANT.md`, `docs/COVERAGE.md` are deleted (`git ls-files docs/` shows only the five format docs) and `AGENTS.md` is mode `120000`.

- [ ] **Step 4: CLAUDE.md budget + symlink**

Run: `wc -l < CLAUDE.md; readlink AGENTS.md`
Expected: `< 100` and `CLAUDE.md`.

- [ ] **Step 5: Commit any fix-ups**

```bash
git add -u && git commit -m 'docs: fix dead links found in final sweep (#64)'
```
(Only if Step 1 required fixes; otherwise report all-green.)

---

## Post-plan

After Task 12, the branch is ready for the finishing-a-development-branch flow (PR against `main`; the docs-only CI skip is gated at job level, so required checks still report). Follow-up outside the PR: enable GitHub private vulnerability reporting in the repo settings.
