# Docs rework — design (issue #64)

## Problem

Documentation is scattered and developer-internal. The README carries deep
technical freight and a dead link to the deleted `docs/ROADMAP.md`; CLAUDE.md
is the de-facto architecture doc and command reference; AGENTS.md is a stale
partial copy of CLAUDE.md (wrong binary crate, dead ROADMAP reference); the
small `docs/` files (DB_CONTRACT, OGG_INVARIANT, COVERAGE) have no clear place
in a coherent set. Known doc rot (e.g. the README's tag-handling limitations
predate binary-tag support) shows the text has drifted from the code.

## Scope

Every document in the repo **except** `docs/superpowers/**` is in scope.
`CHANGELOG.md` and `BENCHMARKS.md` are kept as-is (link fixes only). The three
`contrib/` READMEs get a light touch only: cross-link fixes and any claims
invalidated by the rework.

## End state

Repo root:

| File | Disposition |
| ---- | ----------- |
| `README.md` | Rewritten, usage-first |
| `ARCHITECTURE.md` | New: technical reference |
| `CONTRIBUTING.md` | New: working-developer manual |
| `SECURITY.md` | New: reporting policy |
| `CLAUDE.md` | Rewritten lean (< 100 lines) |
| `AGENTS.md` | Replaced by a symlink to `CLAUDE.md` |
| `CHANGELOG.md` | Kept |
| `BENCHMARKS.md` | Kept |

`docs/` (directly under `docs/`, no subfolder, for discoverability):
`docs/FLAC.md`, `docs/MP3.md`, `docs/M4A.md`, `docs/OGG.md`, `docs/WAV.md`.

Deleted (after their back-validation pass, see Method):

- `docs/DB_CONTRACT.md` → absorbed by ARCHITECTURE.md
- `docs/OGG_INVARIANT.md` → absorbed by `docs/OGG.md`
- `docs/COVERAGE.md` → absorbed by CONTRIBUTING.md
- `docs/ROADMAP.md` is already gone; nothing reintroduces a roadmap. The
  README status text states deferred items inline.

## Method: greenfield + back-validation

Each document is written **from the current code**, not from existing text —
every claim verified against source as it is written. After a document is
drafted, a back-validation pass re-reads the old text it replaces and mines
anything true and worth keeping (hard-won caveats, edge cases); each mined
claim is checked against the code before inclusion. Old docs are deleted only
after their back-validation pass completes.

**CLAUDE.md flag rule:** on CLAUDE.md's back-validation pass, any existing
content the author believes should stay is flagged to the user for a decision
— never silently kept or dropped.

Final sweep: repo-wide markdown link check (kills the dead ROADMAP links and
verifies all new cross-links).

All work happens on a feature branch in a dedicated git worktree.

## Document designs

### README.md — usage-first

1. Hook: one-paragraph pitch (re-tagged, reorganized view; original audio
   bytes never copied or modified). CI badge stays.
2. Quick start: install (`cargo install musefs`), `scan`, `mount`, the result.
3. What it's for: short scenarios — clean view of a messy library, tag
   experiments without touching files, beets/Picard as live tag editors
   (links to `contrib/*` READMEs).
4. Usage: `scan` / `scan --revalidate` / `mount`; template syntax (`$field`,
   `${field}`, fallbacks); the two modes in one sentence each; tuning-flags
   table.
5. Supported formats: a small table (format, what's synthesized, link to
   `docs/<FMT>.md`). Tag-handling detail lives in the format docs; README
   keeps a 2–3 line summary.
6. FAQ (written fresh): does it ever write to my files (no — read-only by
   construction); where edited tags live (the SQLite store, via the plugins
   or SQL); do edits appear without remounting (yes); platform support
   (Linux + FUSE); can I write through the mount (**no — and not planned**;
   out-of-band editing against the store is the design, a permanent
   non-goal, not a deferral); performance on NFS/HDD (tuning flags).
7. Requirements / Status / License. Status states deferred items inline;
   links out to ARCHITECTURE.md and CONTRIBUTING.md.

### ARCHITECTURE.md — technical reference

1. Design overview: the cardinal invariant and one-paragraph serving model
   (synthesized metadata spliced ahead of positioned backing reads).
2. Crate layout: the layered workspace, dependency direction, layer placement
   rules (core integrates; fuse/cli/binary thin).
3. The segment model: `RegionLayout`, the five `Segment` variants, how
   `read_at` walks them. One paragraph per format + link to its doc.
4. Mount modes: `Synthesis` vs `StructureOnly`, FUSE passthrough (kernel
   6.9+, CAP_SYS_ADMIN gating, fallback semantics).
5. The SQLite store: schema shape, migrations policy, and the
   external-writer contract (absorbs DB_CONTRACT.md: scanner-owned columns
   vs external-writer tables; behavior when the contract is violated).
6. Freshness: `content_version` vs `data_version`, HeaderCache keying,
   debounced single-flighted refresh, `--keep-cache` invalidation.
7. Virtual tree: template rendering, collision disambiguation, persistent
   path→inode allocator and inode stability across rebuilds.
8. Scanning: ingest pipeline; what `--revalidate` preserves/prunes/GCs.
9. The contrib ecosystem (short): python-musefs as the store-contract
   library, beets/Picard as writers, the generated-schema mechanism.
   Details stay in contrib READMEs.

### CONTRIBUTING.md — working-developer manual

1. Getting set up: toolchain, FUSE prerequisites,
   `git config core.hooksPath .githooks`; the pre-commit hook runs the full
   workspace test suite (red-test commits always rejected).
2. Build & test: full command reference — workspace/per-crate/substring,
   clippy/fmt, FUSE e2e (`--ignored`, `/dev/fuse`; passthrough e2e needs
   CAP_SYS_ADMIN via sudo on the prebuilt test binary).
3. Test tiers beyond `cargo test`: property tests (`fuzzing` feature);
   coverage-guided fuzzing (nightly, out-of-workspace caveat — fuzz targets
   break only in CI's smoke job, `cargo +nightly fuzz build` locally); the
   mutagen interop suite; the in-diff mutation gate. Mutation-gate TMPDIR
   nuance: tmpfs is fine (and preferable, faster) for small in-diff mutant
   sets; the cgroup + on-disk-TMPDIR recipe is for larger sets where
   allocation-bomb mutants can OOM the host; sharding support exists but
   that workflow hasn't been built out. Keep the empty-diff false-pass
   warning and the no-pipe (exit-code masking) warning.
4. Coverage (absorbs COVERAGE.md): cargo-llvm-cov locally, Codecov in CI,
   why musefs-fuse is excluded.
5. Code conventions: error-handling idioms (per-crate thiserror enums, no
   discarded diagnostics, anyhow only in the CLI), the integer-conversion
   convention, lint policy location, layer-placement rule; benches/ and
   crate tests/ hold API consumers — compile-check with
   `clippy --all-targets`.
6. Adding a format: probe + `synthesize_layout`, the `Format` enum,
   reader/scan wiring, the full test surface (fixture, fuzz target + seed,
   proptest, interop manifest row), and "write `docs/<FMT>.md`".
7. Python plugins: how to run the three contrib test suites, including the
   venv (beets) and system-package (Picard/Qt) gotchas.
8. PRs & commits: conventional-style subjects, scoped commits, the required
   CI aggregator checks, benchmarks recorded in BENCHMARKS.md.

### docs/{FLAC,MP3,M4A,OGG,WAV}.md — per-format, both audiences

One shared shape:

1. Scope line: containers/codecs covered (OGG: Opus, Vorbis, FLAC-in-Ogg;
   multiplexed/chained detected and skipped. M4A: M4A/M4B).
2. What round-trips (user-facing): text tags, canonical-vocabulary mapping,
   the format's extension slot, casing preservation, art handling.
3. Lossy edges (user-facing): **derived from current code and tests at
   writing time** — not from the old README list, which predates binary-tag
   support. The old list resurfaces only via back-validation with per-claim
   code checks.
4. How synthesis works (developer-facing): the layout this format produces,
   segment by segment. `docs/OGG.md` absorbs OGG_INVARIANT.md (invariant
   statement + its "verified by" list).
5. Quirks & invariants: remaining hard-won format-specific facts.

### SECURITY.md

Supported versions (latest release); report privately via GitHub's private
vulnerability reporting (link to the repo's advisories page); what to expect
(acknowledgment, fix, credit); one musefs-specific paragraph: the threat
surface is parsing untrusted media files at scan time and serving them at
read time — fuzz/property suites target exactly this, and parser DoS findings
are in-scope vulnerabilities. Enabling the GitHub repo setting is a follow-up
outside the PR.

### CLAUDE.md — lean rewrite (< 100 lines)

Filter: declarative facts the agent needs on every task, nothing with a
better home elsewhere.

1. What this is: read-only passthrough FUSE filesystem; the cardinal
   invariant (never relax it); SQLite store as source of truth.
2. Style line: "musefs is written in clean, performant, idiomatic Rust" +
   the layer-placement rule.
3. Everyday commands: build, test (workspace/per-crate/substring),
   clippy `--all-targets`, fmt.
4. Pointers table (one line each): ARCHITECTURE.md, CONTRIBUTING.md,
   `docs/<FMT>.md`, store contract location.
5. Repo-operational facts that live nowhere else naturally: pre-commit runs
   the full test suite; FUSE e2e is `--ignored`; contrib Python gotchas
   (one line + link); schema changes regenerate `schema.py` (one line +
   link).

Anything currently in CLAUDE.md not covered by 1–5 moves out or dies
(subject to the flag rule above).

### AGENTS.md

Deleted as a file; recreated as a symlink to CLAUDE.md so cross-tool agents
get the same instructions with zero drift.

## Verification

- Repo-wide markdown link check passes (no dead intra-repo links).
- Every absorbed doc's content is accounted for: absorbed claims verified
  against code, or consciously dropped.
- CLAUDE.md under 100 lines; AGENTS.md resolves through the symlink.
- `rg ROADMAP` over tracked files returns nothing outside `docs/superpowers/`
  (historical references in superpowers specs/plans are fine).

## Out of scope

- `docs/superpowers/**` (specs/plans history).
- Content changes to CHANGELOG.md and BENCHMARKS.md beyond link fixes.
- Substantive rewrites of the contrib READMEs.
- Enabling GitHub private vulnerability reporting (repo setting, not a file).
- A new ROADMAP document.
- Mutation-gate sharding workflow development (mentioned in CONTRIBUTING as
  not yet built out).
