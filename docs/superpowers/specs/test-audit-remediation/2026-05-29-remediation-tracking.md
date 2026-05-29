# Test-Audit Remediation — Tracking Doc

**Source audit:** `docs/audits/2026-05-29-test-audit.md`
**Created:** 2026-05-29
**Status:** Phase 1 complete (harness merged, inventory filled from CI run 26632110192); Phase 2 complete (Ogg hardening); Phase 3a (FLAC) complete; Phase 3b complete (MP3 survivors killed); Phase 3c (MP4) complete; Phase 3d (WAV) complete; phase 4 pending

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model. Its findings are **leads,
not facts.** Every item below is re-verified against the actual code before any
fix is written. Two audit prescriptions were already found wrong during phase-1
brainstorming (see Phase 1, Component A) — treat the rest with the same
suspicion.

## Scope decision

Agreed scope: **everything actionable** — all P1 + all P2 findings (§12 of the
audit) except the two document-only items, **plus a full mutation sweep** that
drives mutation score up across the three logic-bearing crates — `musefs-db`,
`musefs-core`, `musefs-format` — including the 7 format files the audit's partial
run never reached (§9).

**Mutation scope excludes `musefs-cli` and `musefs-fuse`** (the other two
workspace members, `Cargo.toml:3`), by decision:

- `musefs-cli` is thin clap/dispatch glue (30.9% line coverage, expected for a
  binary crate; audit §6) — little mutable logic, exercised end-to-end.
- `musefs-fuse` is a thin `fuser` adapter validated by `#[ignore]`d e2e mounts,
  not llvm-cov/mutant instrumentation (the project's FUSE coverage strategy;
  audit §6/§7). Mutation testing it would require a real mount per mutant.

If either crate later grows non-trivial logic, mutation coverage for it gets its
own follow-up — it is **deferred, not silently dropped.**

## Decomposition into phased sub-projects

Each sub-project gets its own spec → plan → implement cycle. Phase 1 is the only
hard prerequisite; 2–4 may run in any order (or parallel) once phase 1's verified
survivor inventory exists.

```
Phase 1 ──> Phase 2 (Ogg)
        ├─> Phase 3 (Format non-Ogg)
        └─> Phase 4 (Core & DB)
```

### Phase 1 — Quick fixes & mutation-discovery harness  ⟶ STATUS: complete

**Survivor inventory seeded** from CI run 26632110192 (db 5m / core 16m /
format 1h56m): 297 survivors (286 missed + 11 timeout) routed to phases 2–4 in
`2026-05-29-mutation-inventory.md`. Follow-up flagged: sub-shard `musefs-format`
(`--shard k/n`) — it was 75% of the run's wall-clock as a single serial leg.


Fix the `metrics` compile error (A1, unblocks `cargo test --features metrics`),
close the beets FK correctness gap (A2), and produce the data phases 2–4 consume.

- **A. Corrected quick fixes**
  - `metrics.rs:177` — delete stale `backing_mtime_secs: 0,` (audit said
    "rename"; that would duplicate the existing `backing_mtime`). Finding #13.
  - `contrib/beets/tests/test_plugin.py` — route the 6 raw
    `sqlite3.connect(db_path)` calls (115/141/181/197/219/242) through
    `_core.connect()` (which sets `foreign_keys=ON`); add an FK-on regression
    assertion. Audit's "add PRAGMA to `db_path` fixture" is ineffective — that
    connection is a throwaway and the pragma is per-connection. Finding #6.
- **B. Mutation harness** (mirrors `fuzz.yml`)
  - `scripts/mutants.sh` — canonical invocation: per-crate `TMPDIR` child off the
    `/tmp` tmpfs (cargo-mutants 27.0.0 has no `--target-dir`), `--jobs 1`, one
    crate at a time with its scratch removed before the next — never `cargo
    clean`, so the primary `target/` dep cache stays warm (local disk is tight:
    7.3 GB free, `target/` ~5.6 GB).
  - `.github/workflows/mutants.yml` — PR job: `cargo mutants --in-diff` on
    changed Rust files. Scheduled (cron) + `workflow_dispatch` job: full per-crate
    matrix on **stable** (cargo-mutants needs no nightly/llvm-tools), no time cap,
    uploads survivor reports.
- **C. Verified survivor inventory** —
  `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`,
  seeded from a manually dispatched CI run (GitHub runner has disk headroom;
  local does not).
  Supersedes the audit's partial §9. Records structural tool limits to revisit
  (no `Default for Db`; `Ok(Default::default())` unviables).
- **D. This tracking doc.**

### Phase 2 — Ogg hardening  ⟶ STATUS: complete

P1 + all Ogg-related P2 + Ogg mutant kills. Findings #1, #2, #3, #4, #8, #14
(#7 dropped — see below).

- `serve()` unit tests incl. boundaries (#1, #8)
- independent Ogg oracle materializing `Segment::OggAudio`, CRC-verifying across
  Opus/Vorbis/OggFLAC (#2)
- `build_index` consume-mismatch error path (#3)
- `build_index` CRC/continued-page assertions (#4)
- CRC edge cases (#7) — **dropped**: `ogg/crc.rs` had 0 mutation survivors; no CRC
  correctness work (see phase-2 design)
- EOS preservation, reframed from "FLAG_EOS handling" (#14)
- kill surviving `ogg/` + `ogg_index` mutants (from phase-1 inventory)

### Phase 3 — Format-layer coverage & mutants (non-Ogg)  ⟶ STATUS: in progress

Findings #5, #16.

- **3a done** — FLAC survivors killed (equivalents: disjoint-bitfield `| → ^` at
  `:50/:51/:99/:200/:201/:290/:291` and inclusive-bound `> → >=` at
  `parse_picture_block:237/:245`); finding #16 resolved by **skipping zero-byte art
  at FLAC synthesis** (small production fix in `flac.rs::synthesize_layout`), with a
  cross-cutting follow-up noted: apply the same skip to mp3/mp4/ogg/wav synthesis
  (their sub-phases) or filter empty art once at ingestion (`scan.rs`); finding #5
  broadened on FLAC (partial/seam/art windows), with the non-FLAC dimension tracked
  into 3b/3c/3d.
- **3c done** — MP4 survivors killed (40 missed → killed, 4 timeout →
  timeout-detected; no equivalents). Covers `box_header`, `read_box`,
  `read_structure_from`, `read_freeform`, `read_tags`, `read_pictures`,
  `build_udta`, `patch_chunk_offsets`, `synthesize_layout`. The `|` mutants
  (`read_structure_from` `|= → &=` dup-accumulators) are killed, not equivalent.
  Finding #16 resolved for MP4 by **skipping zero-byte art at synthesis** (the first
  *nonempty* art wins in `mp4.rs::synthesize_layout`, mirroring the FLAC fix), and
  finding #5 broadened onto the M4A synthesis path (`write_m4a` helper + four
  `proptest_read_fidelity` M4A cases: backing-audio identity, partial windows,
  header seam, art window).
- **3d done** — WAV survivors killed (24 kills; equivalents: `walk_chunks:49`
  guard-to-true, `synthesize_layout:186` `> → ==` and `> → >=`,
  `synthesize_layout:227` `> → >=`); finding #16 resolved for WAV by **delegating to
  the shared `build_id3v2_segments`, which already skips zero-byte art** (added in
  3b), so no WAV-local filter is needed; finding #5 broadened on WAV
  (read-fidelity proptests: preserve-backing, partial-windows, header-seam, art-window).
- broaden `proptest_read_fidelity` (random offsets, header/audio boundary, art,
  non-FLAC) (#5)
- zero-byte art boundary (#16)
- kill flac/mp3/mp4/wav boundary + bitwise survivors (from phase-1 inventory)
- 3b done — MP3 survivors killed; equivalents = disjoint-shift `| → ^` in
  `synchsafe_decode` and v2.2 24-bit decode (note: disjoint `| → &` are killed,
  not equivalent). Production change: finding #16 zero-byte-art skip applied to
  `mp3.rs::build_id3v2_segments` (mirrors the FLAC fix; also covers the WAV `id3 `
  chunk, which shares this builder). #16 still open for ogg (mp4 closed in 3c).
  Finding #5: the
  MP3 read-fidelity dimension is now done — added `write_mp3` to the core test
  harness and ported the four `proptest_read_fidelity` invariants (whole-audio,
  partial windows, header seam, art window) to MP3; the WAV/MP4 dimensions land in
  their own phases.

### Phase 4 — Core & DB coverage & mutants  ⟶ STATUS: pending

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
| #7 CRC edge cases | 2 | dropped — `ogg/crc.rs` 0 survivors (phase-2 design) |
| #8 serve() boundaries | 2 | |
| #9 probe fallbacks | 4 | |
| #10 tracks.rs SQL | 4 | |
| #11 art.rs races | 4 | |
| #12 tags.rs GROUP BY | 4 | |
| #13 metrics compile error | 1 | audit fix wrong — see Component A |
| #14 EOS flag | 2 | |
| #15 ESTALE | 4 | document-only |
| #16 zero-byte art | 3 | |
