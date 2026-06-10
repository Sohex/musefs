# Release-process hardening design

**Date:** 2026-06-10
**Status:** Draft (brainstorming output; revised after spec-plan-reviewer pass)

## Purpose

Harden the musefs release process ahead of cutting v1.0.0 so that a tag push
cannot ship a partially-verified or partially-distributed release, and so the
procedure is documented rather than tribal. This is the "release-process
stream" identified during the pre-v1.0.0 issue triage.

## Scope

Five issues, delivered as three components:

| Component | Issues | Primary surface |
| --------- | ------ | --------------- |
| 1. Restructured Rust release graph | #164, #222, #163 | `.github/workflows/release.yml`, `.github/workflows/coverage.yml` |
| 2. Automated Lidarr real-instance gate | #224 | new `.github/workflows/lidarr-smoke.yml` + `release-python.yml` |
| 3. Rust `v*` release procedure docs | #223 (+ #162 as a documented step) | `CONTRIBUTING.md` |

#162 (workspace version bump) is a one-shot manual pre-flight action, not a
workflow change, so it is covered as a documented step in Component 3 rather
than as code.

The three components land as **three PRs** off the stream branch. Components 1
and 2 are workflow-only and share no files. Component 3 is docs-only and owns
**all** `CONTRIBUTING.md` edits — including replacing the prose Lidarr
release-gate line (`CONTRIBUTING.md:365`) with a pointer to the Component 2
workflow — so the two `CONTRIBUTING.md` touch points live in one PR and don't
collide. Component 3 has a soft content dependency on 1 and 2 (it documents
both) and is finalized last.

### Out of scope

- The other pre-v1.0.0 release blockers #184 (PyPI trusted-publisher setup) and
  the broader contract/test-hardening streams (#200/#201/#203, #204/#208/#209).
- The Lidarr **download-client** import path (`AlbumImportedEvent` /
  `On Release Import`, which only fires for `NewDownload` imports). It remains a
  documented, manually-exercised gap, as the 2026-06-07 checklist already
  records.

## Component 1 — Restructured release.yml (#164 + #222 + #163)

### Problem

`release.yml` today has four jobs forming two disconnected chains:

- `publish` (crates) verifies tag == workspace version, then publishes crates in
  dependency order — depending on nothing else.
- `build` (matrix) → `smoke` (matrix) → `release-assets`, a separate chain.

Consequences:

- A `v*` tag can publish crates to crates.io before the prebuilt binaries pass
  smoke (#222).
- GitHub release assets can be created even if crate publishing fails, and vice
  versa — a release can be partially shipped across channels (#222).
- The tag-triggered workflow never confirms the tagged commit actually passed
  main CI; this is correct only by convention (#164).
- The crate publish loop assumes immediate crates.io index visibility; on a
  first publish or version bump a dependent crate can fail to resolve the
  just-published crate, leaving a partial release (#163).

### Design

One ordered DAG:

```
gate ──► build (matrix) ──► smoke (matrix) ──► publish (crates) ──► release-assets
```

**`gate`** (new, runs first — fail fast before the build matrix):

1. Tag-equals-workspace-version check (moved here from `publish`).
2. **#164 CI-green gate — gate on the tag's own fresh CI + coverage run.**
   - `ci.yml` already triggers on `tags: ['v*']` (`ci.yml:6`); **add the same
     `tags: ['v*']` trigger to `coverage.yml`** (today it triggers only on
     `push: branches: [main]` + `pull_request`, so `coverage-ok` is *not*
     produced for a tag push — without this change the gate can never see it).
   - The `gate` job **polls** the GitHub Checks API
     (`gh api repos/$GITHUB_REPOSITORY/commits/$SHA/check-runs`) for the tag's
     commit SHA until both required aggregator checks — `ci-ok` and
     `coverage-ok` — reach `status == completed`, then asserts
     `conclusion == success` for each.
   - **Selection rule** (the API returns *all* check-runs of a name across runs,
     and re-runs add new ones): for each of `ci-ok`/`coverage-ok`, take the
     **latest by `completed_at`**; an `in_progress`/`queued` run is *waited on*,
     not treated as failure. Fail closed only on: a completed non-success
     conclusion, or the poll **timeout** (bound: 45 min, covering the full
     matrix incl. the FreeBSD VM e2e; interval ~20 s) with the check still
     absent or incomplete.
   - Because the gate verifies the tagged tree passed the full suite *now*, it
     does not need to assert the commit is an ancestor of `main` — "this exact
     tree is green" is strictly stronger than "this commit was once on main."
   - Job permissions: `contents: read` + `checks: read`; uses the workflow
     `github.token`.

**`build`** → `needs: gate`. Matrix otherwise unchanged. (Note: the release's
own `build`/`smoke` thus start only after CI's matrix is green — the duplicate
build cost is accepted for the safety of one ordered graph.)

**`smoke`** → `needs: build`. Unchanged.

**`publish`** (crates) → **`needs: smoke`** (today: nothing).
- **#163 index-propagation waits.** After each `cargo publish -p <crate>
  --locked`, poll until that exact `<name>@<version>` resolves before publishing
  the next dependent crate. **Probe:** the sparse index
  (`https://index.crates.io/<dir>/<crate>` JSON, checked for the `vers` entry) —
  cheaper and more direct than `cargo search`. Bound: 10 min per crate,
  interval ~10 s; fail the job on timeout.
- **Re-run idempotency (decided).** Before publishing each crate, check whether
  `<name>@<version>` already resolves from the index; if so, **skip** it.
  `cargo publish` of an already-published version *errors*, so without this a
  whole-workflow re-run after a mid-loop failure would die on crate 1 and leave
  the release partially shipped (the exact #222 failure mode). Skipping makes a
  re-run safe and idempotent, reusing the #163 index check. Publish order is
  unchanged (`musefs-db musefs-format musefs-core musefs-fuse musefs-cli
  musefs`).

**`release-assets`** → **`needs: [smoke, publish]`** (today: only `smoke`).
GitHub assets upload only after crates.io publishing succeeds. Upload remains
idempotent on re-run (`gh release ... --clobber`).

### Ordering rationale

crates.io publishing is effectively irreversible (yank-only), so it is the last
mutation, performed only after both the CI-green `gate` and binary `smoke` give
maximum confidence. GitHub release assets (re-uploadable) follow crate publish,
so a failure there leaves crates published but assets retriable, never a GitHub
release pointing at a version absent from crates.io.

## Component 2 — Automated Lidarr real-instance gate (#224)

### Problem

`CONTRIBUTING.md` states "The Lidarr real-instance smoke test is a release
gate, not a default CI job," but that gate exists only as prose. The default
Lidarr CI job runs fixture-level tests; neither CI nor `release-python.yml`
proves the real Lidarr Custom Script integration still works. A Python package
release can therefore be cut without the integration the docs call out as
release-gated ever being exercised.

### Key architectural fact (verified against the code)

On an `AlbumDownload` event the Custom Script (`musefs-lidarr-import`) receives
`Lidarr_Album_Id` and `Lidarr_AddedTrackPaths`, then queries **Lidarr's REST
API** (`LidarrClient`, `contrib/lidarr/src/musefs_lidarr/api.py`) for
`track_files`/`tracks`/`album`/`artist` scoped by album id
(`sync.py` `collect_event_payloads`, `mapping.records_for_paths`) and writes
*that* metadata into the musefs store. The rich tags do **not** come from the
event env vars — they come from Lidarr's database. So for tag assertions to
mean anything, the real Lidarr instance must actually hold an artist/album/
tracks, and Lidarr populates its DB from MusicBrainz (`api.lidarr.audio`).

The Lidarr **Test event** carries no `Album_Id`/`SourcePath`, so it only proves
that the real Lidarr process execs the script and the lowercased-env-var path
resolves (the `StringDictionary` case bug the 2026-06-07 run caught). The
**content** assertions are driven entirely by the *constructed* `AlbumDownload`
env (step 5 below), not by Lidarr firing `AlbumDownload` itself (it cannot
without a download-client import — the out-of-scope gap).

### Design

A new `.github/workflows/lidarr-smoke.yml`, dispatchable (`workflow_dispatch`)
and reusable (`workflow_call`). Seed mechanism: **API-driven, accepting a
one-time live `api.lidarr.audio` metadata fetch** — chosen for schema-stability
across Lidarr versions (it uses the same API surface the script exercises,
rather than raw SQL coupled to an EF-Core schema). The cost, accepted
explicitly: the seed step has a **network dependency** on `api.lidarr.audio`,
so the gate can flake on upstream outage/rate-limit; the metadata-add step gets
a bounded retry and a clear failure message distinguishing "upstream metadata
unavailable" from "integration broken."

Steps:

1. **Generate** synthetic FLAC tracks with ffmpeg via a committed harness
   script, laid out as `Artist/Album/NN Title.flac` under a directory that is
   **bind-mounted into the Lidarr container** (runnable in-tree script; no
   committed binary fixtures — large artifacts stay gitignored).
2. **Boot the pinned `linuxserver/lidarr` container** (pinned by image digest
   for reproducible Lidarr *behavior*) with `--device /dev/fuse --cap-add
   SYS_ADMIN --security-opt apparmor=unconfined` (the precedent is the release
   Alpine smoke, `release.yml:144-148`, which runs `docker run` with exactly
   these flags on a stock `ubuntu-latest` runner).
3. **Seed via Lidarr's API:**
   - Set a root folder = the bind-mounted FLAC directory (in-container path).
   - Set the two **safe-settings** config rows the script's preflight enforces
     (`config/metadataprovider` `writeAudioTags == no`;
     `config/mediamanagement` `fileDate == none`, permissions-setting off) —
     `api.py` `run_preflight`/`check_safe_settings` *refuses to proceed* on
     violation, so without seeding these the smoke aborts at preflight instead
     of reaching its assertions.
   - Add a fixed artist by MusicBrainz id (one-time live `api.lidarr.audio`
     fetch), then trigger a scan / manual import so Lidarr creates
     album/track/`trackfile` rows whose `path` values are the **in-container
     realpath** of the generated FLACs.
4. **Configure** the Custom Script connection via the API and **fire the Test
   event**; assert the real Lidarr execs `musefs-lidarr-import` and the
   lowercased-env resolution succeeds. (This proves exec only — no content.)
5. **Invoke** the script with a constructed `AlbumDownload` env: `Album_Id` =
   the seeded album, `AddedTrackPaths` = the **in-container realpath** of the
   FLACs (must match Lidarr's seeded `trackfile.path`; `mapping.match_track_file`
   compares both sides via `realpath_key()`, so a host/container path-namespace
   mismatch would silently skip every track). The script queries the seeded
   Lidarr API and writes tags to the musefs store + creates symlinks.
6. **Assert:** symlinks created for every track; store tags match the metadata
   Lidarr returned; backing audio bytes unchanged (sha256 before/after); the
   served mount carries the tags; and — to defend against vacuous passes —
   **`records > 0` and `skipped == 0`** (a path-namespace mismatch must fail
   loud, not pass green).

### Enforcement and triggers

- **Required gate in `release-python.yml`:** add a `lidarr-smoke` job that
  `uses:` the reusable workflow, and add it to the `publish` job's `needs`
  (today `needs: [test-python-musefs, test-beets, test-lidarr, test-picard]`,
  `release-python.yml:138`). A Python package release cannot publish without it
  green — the non-forgettable mechanism #224 requires.
- **PR coverage:** also run on PRs touching `contrib/lidarr/**` or the musefs
  binary, so the gate stays continuously green rather than firing only at
  release time.
- **Documented gap:** the download-client → `AlbumImportedEvent` path stays
  manual; the checklist records it as a known gap.

## Component 3 — Rust `v*` release procedure docs (#223, incl. #162)

### Problem

`CONTRIBUTING.md` documents the Python `py-v*` flow but has no equivalent for
the Rust `v*` flow, even though that flow now has crates.io publishing,
prebuilt cross-compiled binaries, smoke tests, and GitHub release assets.
Cutting v1.0.0 currently relies on tribal knowledge for ordering, propagation
retries, and partial-failure recovery.

### Design

Add a "Releasing the Rust crates and binaries" section to `CONTRIBUTING.md`,
mirroring the existing Python section's structure and tone. The workflow is the
source of truth; the doc is the human checklist, not a re-explanation of the
YAML. This PR also **replaces the prose Lidarr release-gate line**
(`CONTRIBUTING.md:365`) with a pointer to the Component 2 workflow, keeping all
`CONTRIBUTING.md` edits in one place.

Contents:

1. **Pre-flight** — clean tree; confirm current `main` is green; required
   secrets/permissions present (`CARGO_REGISTRY_TOKEN`). Note that the tag will
   trigger a fresh full `ci.yml` + `coverage.yml` run that the release `gate`
   job waits on, so a broken tree blocks the release automatically.
2. **Version bump (#162)** — pick `X.Y.Z`; bump the workspace `version`; bump
   all internal `musefs-*` path-dependency constraints off the previous version;
   promote `CHANGELOG.md` `[Unreleased]` → `[X.Y.Z] - <date>`. Run a dry-run
   check (`cargo package --locked` per crate) before tagging — noting this
   catches packaging errors but *not* the cross-crate index-propagation problem
   (it uses path deps), which is what the in-workflow #163 wait handles.
3. **Tag & push** — exact `git tag vX.Y.Z` / push commands; the tag push starts
   both CI and the release workflow, and the `gate` job blocks publishing until
   `ci-ok` + `coverage-ok` are green on the tagged tree.
4. **What `release.yml` does** — the ordered DAG
   (`gate → build → smoke → publish → release-assets`), including the #163
   index-propagation waits and the skip-if-already-published re-run behavior, so
   a releaser knows what to expect and that re-running after a partial failure
   is safe.
5. **Retry / rollback** — crates.io is yank-only (cannot un-publish). Because
   the publish loop skips crates already in the index, re-running the workflow
   after a mid-loop failure resumes cleanly and then runs `release-assets`.
   Asset upload re-runs via `--clobber`.
6. **Post-release verification** — `cargo install musefs`; download a release
   binary and `sha256sum -c`; confirm all four target tarballs + checksums are
   attached to the GitHub release.
7. **Cross-reference the Lidarr gate** (Component 2) — note that it gates the
   Python `py-v*` release, and that a v1.0.0 milestone should ensure both the
   Rust and Python flows (and the Lidarr gate) are run.

## Testing / verification

- **Component 1:** the `gate` logic is the part to verify carefully. Acceptance
  evidence: a dry-run of the gate's check-run query + selection against (a) a
  commit with green `ci-ok`/`coverage-ok`, (b) one with a failed check, and
  (c) one with an in-progress check (must wait, not fail). The publish-loop
  skip/`#163`-wait is exercised by the existing smoke matrix; verify the sparse
  index probe and the skip-if-present branch against a known-published version.
- **Component 2:** the `lidarr-smoke.yml` job is itself the test; its PR-trigger
  on `contrib/lidarr/**` gives continuous signal. Acceptance is a green run
  demonstrating all assertions in step 6, including `skipped == 0`. Because the
  seed step depends on `api.lidarr.audio`, an upstream-metadata failure must
  surface as a distinct, recognizable error (not a generic red) so a flake is
  not mistaken for an integration regression.
- **Component 3:** docs; verified by review against the actual `release.yml`
  DAG and the #162 steps. No automated test.

## Sequencing within the stream

Three PRs. Components 1 and 2 are independent (workflow-only, disjoint files)
and can proceed in parallel. Component 3 (docs) documents both and owns all
`CONTRIBUTING.md` edits, so it is finalized after 1 and 2 settle, though it can
be drafted in parallel.

## Decisions resolved during review

- **CI-green gate:** run `ci-ok` + `coverage-ok` on the tag (add the tags
  trigger to `coverage.yml`) and have `gate` poll-and-wait, selecting the
  latest-completed run per name and failing closed only on a completed
  non-success or timeout. Dissolves the tag-on-main question.
- **Publish-loop idempotency:** skip crates already resolvable from the index,
  reusing the #163 probe, so whole-workflow re-runs are safe (closes #222 on
  re-run).
- **Lidarr seed mechanism:** API-driven, accepting a one-time live
  `api.lidarr.audio` fetch (schema-stable; network-flake risk accepted and
  surfaced as a distinct error). Seed must also set the preflight safe-settings
  config rows, and path-matching must assert `skipped == 0`.
- **CONTRIBUTING.md ownership:** all edits (new Rust section + the `:365` Lidarr
  gate line) live in Component 3's PR.
