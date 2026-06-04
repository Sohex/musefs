# PR 6 Interop And DB Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Document and test the SQLite external-writer contract, and make interop tests verify audio payload preservation.

**Architecture:** Use the real musefs schema and real synthesized fixtures. The interop manifest must carry separate source and synthesized audio ranges because synthesized metadata changes output offsets.

**Tech Stack:** Rust interop emitter, Python pytest, mutagen, SQLite.

---

### Task 1: Document DB Ownership Contract

**Files:**
- Create: `docs/DB_CONTRACT.md`

- [ ] **Step 1: Add contract doc**

Create documentation that says:
- scanner owns `tracks` structural fields;
- external writers may write `tags`, `art`, and `track_art`;
- external tools should call `musefs scan` to create/update structural rows;
- invalid structural rows are handled as backing/layout errors, not trusted.

### Task 2: Test External-Writer Misuse Against Real Schema

**Files:**
- Test: `musefs-db/tests/external_contract.rs` or `musefs-core/tests/external_contract.rs`

- [ ] **Step 1: Add real-schema misuse test**

Use `musefs_db::Db::open` on a temp file, insert a valid scanned track through
`upsert_track`, then mutate a scanner-owned field directly through a test-only
SQL helper or public DB connection helper. The test must prove one of:
- SQLite constraints/triggers reject the mutation; or
- `HeaderCache::resolve`/`Musefs` returns a controlled error such as
  `BackingChanged`/format error rather than panicking or serving invalid bytes.

Do not create a hand-written mini schema.

### Task 3: Emit Source And Synthesized Audio Ranges

**Files:**
- Modify: `musefs-core/tests/interop_emit.rs`

- [ ] **Step 1: Extend manifest rows**

For every emitted fixture, include:

```json
{
  "file": "out.flac",
  "source_file": "src.flac",
  "title": "Interop Title",
  "artist": "Interop Artist",
  "source_audio_offset": 42,
  "source_audio_length": 400,
  "synth_audio_offset": 128,
  "synth_audio_length": 400,
  "ogg_payload_only": false
}
```

`source_audio_offset/source_audio_length` come from scan bounds. `synth_audio_*`
must come from the synthesized `RegionLayout`, not by reusing source offsets.
For non-Ogg files, the synthesized audio range is the `BackingAudio` segment
range in output coordinates. For Ogg, either emit enough packet-payload metadata
to compare payloads or set `ogg_payload_only: true` and implement a payload-aware
comparison.

### Task 4: Verify Audio Payloads In Python

**Files:**
- Modify: `tests/interop/test_mutagen_roundtrip.py`

- [ ] **Step 1: Add byte-preservation assertions**

For non-Ogg rows, read source bytes at `source_audio_offset` and synthesized
bytes at `synth_audio_offset`, both for `source_audio_length`, and assert exact
equality.

For Ogg rows, compare packet payload bytes using an Ogg page parser or skip whole
page byte equality with a clear assertion that length-only is not sufficient.
If payload parser work is too large for this PR, keep the Ogg assertion in Rust
where Ogg page helpers already exist and document that Python interop verifies
non-Ogg exact byte preservation.

- [ ] **Step 2: Verify**

Run:

```bash
MUSEFS_INTEROP_DIR=/tmp/musefs-interop cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/musefs-interop python3 -m pytest tests/interop -v
```

Expected: both pass.

- [ ] **Step 3: Commit**

```bash
git add docs/DB_CONTRACT.md musefs-core/tests tests/interop
git commit -m "test(core): verify DB contract and interop audio preservation

Closes #11
Closes #14"
```
