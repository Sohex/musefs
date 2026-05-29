# Mutation Survivor Inventory

**Source:** `mutants.yml` `full` job (CI). Supersedes the audit's partial §9
(which only reached `flac.rs`).
**Scope:** `musefs-db`, `musefs-core`, `musefs-format`. `musefs-cli` /
`musefs-fuse` out of scope by decision (see remediation tracking doc).
**Run:** _PENDING — fill from the first dispatched CI run._

## How to (re)generate

1. Trigger the campaign: GitHub → Actions → **Mutants** → **Run workflow**
   (`workflow_dispatch`), or wait for the Monday cron.
2. Download the `mutants-<crate>` artifacts from the run.
3. In each artifact the per-result lists live under `<crate>/mutants.out/`
   (cargo-mutants writes `caught.txt` / `missed.txt` / `unviable.txt` /
   `timeout.txt` into a `mutants.out/` subdir of the `--output` dir). Transcribe
   those per crate into the tables below.

## Tool limitations to revisit (phase 4)

- `musefs-db`: every mutant replaces a body with `Ok(Default::default())` /
  `Ok(0|1|-1)`; `Db` has no `Default`, so all are unviable. Implementing
  `Default for Db` (phase 4) makes db mutation testing meaningful.
- A few `musefs-format` mutants share the `Ok(Default::default())` unviable
  pattern.

## musefs-db

| File | Caught | Missed | Unviable | Timeout | Notes |
|------|-------:|-------:|---------:|--------:|-------|
| _pending_ | | | | | |

### Surviving mutants → phase

| File:line | Mutation | Phase |
|-----------|----------|------:|
| _pending_ | | |

## musefs-core

| File | Caught | Missed | Unviable | Timeout | Notes |
|------|-------:|-------:|---------:|--------:|-------|
| _pending_ | | | | | |

### Surviving mutants → phase

| File:line | Mutation | Phase |
|-----------|----------|------:|
| _pending_ | | 4 |

## musefs-format

| File | Caught | Missed | Unviable | Timeout | Notes |
|------|-------:|-------:|---------:|--------:|-------|
| _pending_ | | | | | |

### Surviving mutants → phase

| File:line | Mutation | Phase |
|-----------|----------|------:|
| _pending_ (ogg/*) | | 2 |
| _pending_ (flac/mp3/mp4/wav) | | 3 |
