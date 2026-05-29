# Test-Audit Remediation — Tracking Doc

**Source audit:** `docs/audits/2026-05-29-test-audit.md`
**Created:** 2026-05-29
**Status:** Phase 1 in brainstorm/spec

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model. Its findings are **leads,
not facts.** Every item below is re-verified against the actual code before any
fix is written. Two audit prescriptions were already found wrong during phase-1
brainstorming (see Phase 1, Component A) — treat the rest with the same
suspicion.

## Scope decision

Agreed scope: **everything actionable** — all P1 + all P2 findings (§12 of the
audit) except the two document-only items, **plus a full mutation sweep** that
drives mutation score up across all crates, including the 7 format files the
audit's partial run never reached (§9).

## Decomposition into phased sub-projects

Each sub-project gets its own spec → plan → implement cycle. Phase 1 is the only
hard prerequisite; 2–4 may run in any order (or parallel) once phase 1's verified
survivor inventory exists.

```
Phase 1 ──> Phase 2 (Ogg)
        ├─> Phase 3 (Format non-Ogg)
        └─> Phase 4 (Core & DB)
```

### Phase 1 — Quick fixes & mutation-discovery harness  ⟶ STATUS: spec

Unblock the suite and produce the data phases 2–4 consume.

- **A. Corrected quick fixes**
  - `metrics.rs:177` — delete stale `backing_mtime_secs: 0,` (audit said
    "rename"; that would duplicate the existing `backing_mtime`). Finding #13.
  - `contrib/beets/tests/test_plugin.py` — route the 6 raw
    `sqlite3.connect(db_path)` calls (115/141/181/197/219/242) through
    `_core.connect()` (which sets `foreign_keys=ON`); add an FK-on regression
    assertion. Audit's "add PRAGMA to `db_path` fixture" is ineffective — that
    connection is a throwaway and the pragma is per-connection. Finding #6.
- **B. Mutation harness** (mirrors `fuzz.yml`)
  - `scripts/mutants.sh` — canonical invocation: `TMPDIR` off the `/tmp` tmpfs,
    `--jobs 1`, `cargo clean` first, one crate at a time, build dir removed
    between crates (local disk is tight: 7.3 GB free, `target/` ~5.6 GB).
  - `.github/workflows/mutants.yml` — PR job: `cargo mutants --in-diff` on
    changed Rust files. Scheduled (cron) + `workflow_dispatch` job: full per-crate
    matrix, `llvm-tools-preview`, no time cap, uploads survivor reports.
- **C. Verified survivor inventory** —
  `docs/audits/2026-05-29-mutation-inventory.md`, seeded from a manually
  dispatched CI run (GitHub runner has disk headroom; local does not).
  Supersedes the audit's partial §9. Records structural tool limits to revisit
  (no `Default for Db`; `Ok(Default::default())` unviables).
- **D. This tracking doc.**

### Phase 2 — Ogg hardening  ⟶ STATUS: pending phase 1

P1 + all Ogg-related P2 + Ogg mutant kills. Findings #1, #2, #3, #4, #7, #8, #14.

- `serve()` unit tests incl. boundaries (#1, #8)
- independent Ogg oracle materializing `Segment::OggAudio`, CRC-verifying across
  Opus/Vorbis/OggFLAC (#2)
- `build_index` consume-mismatch error path (#3)
- `build_index` CRC/continued-page assertions (#4)
- CRC edge cases (#7)
- EOS flag handling (#14)
- kill surviving `ogg/` + `ogg_index` mutants (from phase-1 inventory)

### Phase 3 — Format-layer coverage & mutants (non-Ogg)  ⟶ STATUS: pending phase 1

Findings #5, #16.

- broaden `proptest_read_fidelity` (random offsets, header/audio boundary, art,
  non-FLAC) (#5)
- zero-byte art boundary (#16)
- kill flac/mp3/mp4/wav boundary + bitwise survivors (from phase-1 inventory)

### Phase 4 — Core & DB coverage & mutants  ⟶ STATUS: pending phase 1

Findings #9, #10, #11, #12, #15.

- scan probe fallbacks (#9) + scan mutants
- reader.rs header-cache survivors
- facade glue survivors
- tree.rs disambiguate timeouts (suspected infinite-loop path)
- db tracks/art/tags SQL-branch coverage (#10, #11, #12)
- document the ESTALE gap (#15)
- decide on `Default for Db` to make db mutation testing viable

## Finding → phase map

| Finding | Phase | Note |
|--------:|:-----:|------|
| #1 serve() no tests | 2 | |
| #2 no Ogg oracle | 2 | |
| #3 consume-mismatch | 2 | |
| #4 build_index gaps | 2 | |
| #5 proptest offset-0 | 3 | |
| #6 beets FK parity | 1 | audit fix wrong — see Component A |
| #7 CRC edge cases | 2 | |
| #8 serve() boundaries | 2 | |
| #9 probe fallbacks | 4 | |
| #10 tracks.rs SQL | 4 | |
| #11 art.rs races | 4 | |
| #12 tags.rs GROUP BY | 4 | |
| #13 metrics compile error | 1 | audit fix wrong — see Component A |
| #14 EOS flag | 2 | |
| #15 ESTALE | 4 | document-only |
| #16 zero-byte art | 3 | |
