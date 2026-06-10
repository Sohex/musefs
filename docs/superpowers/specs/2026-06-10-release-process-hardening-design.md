# Release-process hardening design

**Date:** 2026-06-10
**Status:** Draft (brainstorming output)

## Purpose

Harden the musefs release process ahead of cutting v1.0.0 so that a tag push
cannot ship a partially-verified or partially-distributed release, and so the
procedure is documented rather than tribal. This is the "release-process
stream" identified during the pre-v1.0.0 issue triage.

## Scope

Five issues, delivered as three components:

| Component | Issues | Primary surface |
| --------- | ------ | --------------- |
| 1. Restructured Rust release graph | #164, #222, #163 | `.github/workflows/release.yml` |
| 2. Automated Lidarr real-instance gate | #224 | new `.github/workflows/lidarr-smoke.yml` + `release-python.yml` |
| 3. Rust `v*` release procedure docs | #223 (+ #162 as a documented step) | `CONTRIBUTING.md` |

#162 (workspace version bump) is a one-shot manual pre-flight action, not a
workflow change, so it is covered as a documented step in Component 3 rather
than as code.

The three components are independent enough to land as **three separate PRs**
off the stream branch: the docs PR (Component 3) shares no files with the two
workflow PRs, and `release.yml` (Component 1) and the new `lidarr-smoke.yml` /
`release-python.yml` changes (Component 2) do not touch each other.

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

**`gate`** (new, runs first — cheap, fail fast before the build matrix):

1. Tag-equals-workspace-version check (moved here from `publish`).
2. **#164 CI-green gate.** Resolve the tag's commit SHA and query
   `gh api repos/$GITHUB_REPOSITORY/commits/$SHA/check-runs`. Assert that **both
   `ci-ok` and `coverage-ok`** have `conclusion == success`. Fail closed if
   either check is missing, still pending/queued, or any non-success conclusion.
   A tag on an unverified commit therefore cannot publish.
   - `coverage-ok` is treated as a reliable required check (the 2026-06 Codecov
     GPG-verification break was a one-time upstream issue fixed by the
     codecov-action pin bump in PR #207, not a recurring flake).
   - Job permissions: `contents: read` + `checks: read`; uses the workflow
     `github.token`.

**`build`** → `needs: gate`. Matrix otherwise unchanged.

**`smoke`** → `needs: build`. Unchanged.

**`publish`** (crates) → **`needs: smoke`** (today: nothing).
**#163 index-propagation waits:** after each `cargo publish -p <crate>
--locked`, poll until that exact `<name>@<version>` resolves from the crates.io
index before publishing the next dependent crate. Bounded retry with a timeout;
fail the job on exhaustion. Publish order is unchanged
(`musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs`).
Because `cargo publish` of an already-published version errors, the plan should
decide whether the loop skips crates whose `<name>@<version>` already resolves
from the index (making a whole-workflow re-run after a partial failure safe) or
leaves resume-from-failure to the documented manual procedure (Component 3,
step 5).

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

### Key architectural fact

On an `AlbumDownload` event the Custom Script
(`musefs-lidarr-import`) receives `Lidarr_Album_Id` and
`Lidarr_AddedTrackPaths`, then queries **Lidarr's REST API**
(`LidarrClient`, `contrib/lidarr/src/musefs_lidarr/api.py`) for
`track_files`/`tracks`/`albums_by_id`/`artists_by_id`
(`sync.py` `EventPayloads`, `mapping.records_for_paths`) and writes *that*
metadata into the musefs store. The rich tags do **not** come from the event
env vars — they come from Lidarr's database. So for tag assertions to mean
anything, the real Lidarr instance must actually hold an artist/album/tracks,
and Lidarr populates its DB from MusicBrainz (`api.lidarr.audio`).

The Lidarr **Test event** carries no `Album_Id`/`SourcePath`, so it only proves
that the real Lidarr process execs the script and the lowercased-env-var path
resolves (the `StringDictionary` case bug the 2026-06-07 run caught). Content
assertions need a known album.

### Design

A new `.github/workflows/lidarr-smoke.yml`, dispatchable (`workflow_dispatch`)
and reusable (`workflow_call`):

1. **Generate** synthetic FLAC tracks with ffmpeg via a committed harness script
   (runnable in-tree script; no committed binary fixtures — large artifacts stay
   gitignored).
2. **Boot the pinned `linuxserver/lidarr` container** (pinned by image digest)
   with `--device /dev/fuse --cap-add SYS_ADMIN --security-opt
   apparmor=unconfined` (same flags the release Alpine smoke already uses), and
   apply a **seeded DB fixture**: one synthetic artist + album + tracks, fixed
   MusicBrainz IDs, track paths matching the generated FLACs. The seed is
   produced by a committed script (hand-written SQL or a snapshot-once helper);
   a comment records that the seed must be regenerated when the image digest is
   bumped, because it is coupled to that Lidarr version's schema. This keeps the
   smoke fully offline and deterministic — the real Lidarr serves the seeded
   rows over its own API.
3. **Configure** the Custom Script connection via Lidarr's API and **fire the
   Test event**; assert the real Lidarr execs `musefs-lidarr-import` and the
   lowercased-env resolution succeeds.
4. **Invoke** the script with a constructed `AlbumDownload` env (`Album_Id` +
   `AddedTrackPaths` = the generated FLACs). The script queries the seeded
   Lidarr API and writes tags to the musefs store + creates symlinks.
5. **Assert:** symlinks created for every track; store tags match the seeded
   metadata; backing audio bytes unchanged (sha256 before/after); the served
   mount carries the tags.

### Enforcement and triggers

- **Required gate in `release-python.yml`:** the `publish` job gains a `needs:`
  on this smoke; a Python package release cannot publish without it green. This
  is the non-forgettable mechanism #224 requires.
- **PR coverage:** also run on PRs touching `contrib/lidarr/**` or the musefs
  binary, so the gate stays continuously green rather than firing only at
  release time.
- **Documented gap:** the download-client → `AlbumImportedEvent` path stays
  manual; the checklist records it as a known gap.

The prose `release gate` line in `CONTRIBUTING.md` is updated to reference this
workflow instead of describing an unenforced convention.

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
YAML.

Contents:

1. **Pre-flight** — clean tree; confirm current `main` is green
   (`ci-ok` + `coverage-ok`), because the `gate` job (Component 1) fails closed
   otherwise; required secrets/permissions present (`CARGO_REGISTRY_TOKEN`).
2. **Version bump (#162)** — pick `X.Y.Z`; bump the workspace `version`; bump
   all internal `musefs-*` path-dependency constraints off the previous version;
   promote `CHANGELOG.md` `[Unreleased]` → `[X.Y.Z] - <date>`. Run a dry-run
   check (`cargo package --locked` per crate) before tagging.
3. **Tag & push** — exact `git tag vX.Y.Z` / push commands; note that the tag
   must sit on a CI-green `main` commit or the `gate` job fails closed.
4. **What `release.yml` does** — the ordered DAG
   (`gate → build → smoke → publish → release-assets`), including the #163
   index-propagation waits, so a releaser knows what to expect and where it can
   stop.
5. **Retry / rollback** — crates.io is yank-only (cannot un-publish). Partial
   failure guidance: `cargo publish` of an already-published version *errors*
   (it is not idempotent), so a blind re-run fails on the crates already up.
   Recovery is to resume from the failed crate (publish only the remaining
   crates) — note whether the publish loop should skip already-published
   versions to make whole-workflow re-runs safe, or whether the releaser
   resumes manually. Asset upload, by contrast, re-runs safely via `--clobber`.
6. **Post-release verification** — `cargo install musefs`; download a release
   binary and `sha256sum -c`; confirm all four target tarballs + checksums are
   attached to the GitHub release.
7. **Cross-reference the Lidarr gate** (Component 2) — note that it gates the
   Python `py-v*` release, and that a v1.0.0 milestone should ensure both the
   Rust and Python flows (and the Lidarr gate) are run.

## Testing / verification

- **Component 1:** `release.yml` changes are exercised by the existing
  smoke/build matrix; the new `gate` logic (SHA resolution, check-run query,
  fail-closed behavior) is the part to verify carefully — validate the
  `gh api` query shape and the missing/pending/failed branches. A dry-run of the
  `gate` job logic against a known-green and a known-not-green commit SHA is the
  acceptance evidence.
- **Component 2:** the `lidarr-smoke.yml` job is itself the test; its PR-trigger
  on `contrib/lidarr/**` gives continuous signal. Acceptance is a green run that
  demonstrates all five assertions (symlinks, store tags == seeded metadata,
  bytes unchanged, mounted tags, Test-event exec).
- **Component 3:** docs; verified by review against the actual `release.yml`
  DAG and the #162 steps. No automated test.

## Sequencing within the stream

The three components are independent and can proceed in parallel as three PRs.
Recommended order if serialized: Component 1 (the core graph) first, then
Component 2 (Lidarr gate), then Component 3 (docs), so the docs describe the
already-merged graph. Component 3 has a soft dependency on 1 and 2 being
settled (it documents both), but can be drafted in parallel and finalized last.
