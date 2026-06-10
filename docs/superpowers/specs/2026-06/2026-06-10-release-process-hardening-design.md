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
and 2 are workflow-only; they overlap **only** in appending unit-test steps to
the same `python-musefs` job in `ci.yml` (merge Component 1 first — see
Sequencing). Component 3 is docs-only and owns
**all** `CONTRIBUTING.md` edits — including replacing the prose Lidarr
release-gate line (`CONTRIBUTING.md:365`) with a pointer to the Component 2
workflow — so the two `CONTRIBUTING.md` touch points live in one PR and don't
collide. Component 3 has a soft content dependency on 1 and 2 (it documents
both) and is finalized last.

### Out of scope

- The other pre-v1.0.0 release blockers #184 (PyPI trusted-publisher setup) and
  the broader contract/test-hardening streams (#200/#201/#203, #204/#208/#209).

> **Update (post-design):** Component 2 was first scoped to a mock-API smoke,
> leaving the Lidarr **download-client** path (`AlbumImportedEvent`, which only
> fires for `NewDownload` imports) as a manual gap. That gap was subsequently
> **closed** by a full real-instance e2e (`.github/workflows/lidarr-e2e.yml`,
> `scripts/lidarr-e2e/run-e2e.sh`): local metadata/indexer/qBittorrent mocks
> drive a real Lidarr through a genuine download-client import → `OnReleaseImport`
> → the real musefs scripts, asserting the served file carries Lidarr-supplied
> tags. The two tiers below are the final state: the **mock-API smoke** is the
> fast PR check; the **full e2e** is the release gate on the Python publish.

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

### Key architectural facts (verified against the code)

The integration is split across **two** console scripts
(`contrib/lidarr/pyproject.toml`):

- **`musefs-lidarr-import`** (`cli_import.py`) creates the symlink for one import.
  For any non-Test event it **requires** `Lidarr_SourcePath` and
  `Lidarr_DestinationPath` (`import_link.py:parse_import_env`) and makes the link
  — it never touches the API and never writes tags.
- **`musefs-lidarr-sync`** (`cli_sync.py`) is the tag-writer. On an
  `AlbumDownload` event it reads `Lidarr_Album_Id`/`Lidarr_AddedTrackPaths`,
  runs `run_preflight`, queries **Lidarr's REST API** (`LidarrClient`,
  `api.py`: `/api/v1/config/*`, `/api/v1/trackfile`, `/api/v1/track`,
  `/api/v1/album/{id}`, `/api/v1/artist/{id}`) via `collect_event_payloads`, and
  writes the returned metadata into the musefs store
  (`mapping.build_pairs` emits lowercase keys like `artist`).

So a faithful smoke drives **both**: `-sync` for the store-tag assertions and
`-import` for the symlink assertion. (The earlier draft of this spec wrongly
named `-import` as the API/tag path — corrected here.)

The Lidarr **Test event** carries no `Album_Id`/`SourcePath`, so it only proves
that the real Lidarr process execs the script and the lowercased-env-var path
resolves (the `StringDictionary` case bug the 2026-06-07 run caught).

### Why we don't import a real album into Lidarr

Lidarr only creates `trackfile` rows when its MusicBrainz track-matcher maps a
file to a monitored album track; synthetic ffmpeg tones never match, so a
`RescanArtist` seed produces **zero** trackfiles and the gate would be red on
every run. Forcing matches (ManualImport, MB-matching fixtures) is brittle and
network-coupled to `api.lidarr.audio`. **Decision:** keep a *real* Lidarr only
to prove the **Test-event exec path**, and drive the **content** assertions
against a **local mock Lidarr API** returning fixed JSON for the generated
FLACs. This is deterministic and network-free (no MusicBrainz dependency, so no
upstream-outage release-blocking). Accepted tradeoff: the smoke no longer
catches real-Lidarr REST **schema drift** — that risk is carried by the
`contrib/lidarr` unit suite and the documented manual checklist.

### Design

A new `.github/workflows/lidarr-smoke.yml`, dispatchable (`workflow_dispatch`)
and reusable (`workflow_call`). Steps:

1. **Generate** synthetic FLAC tracks with ffmpeg via a committed harness script
   (runnable in-tree script; no committed binary fixtures).
2. **Real-instance exec proof.** Boot the pinned `linuxserver/lidarr` container
   (pinned by digest), **bind-mounting the plugin source** (`contrib/lidarr/src`,
   `contrib/python-musefs/src`) and a committed wrapper
   (`scripts/lidarr_import_wrapper.sh`) into it. The Alpine/.NET image ships no
   python3 and cannot see host-installed scripts, so: `docker exec` an
   `apk add --no-cache python3` (~2s), point a Custom Script connection at the
   in-container wrapper path, and **fire the Test event**. The wrapper runs the
   real `musefs-lidarr-import` from the bind-mounted source under the container's
   python3; a success return proves real Lidarr execs the real script and that
   `lidarr_get` resolves Lidarr's lowercased env keys (the `StringDictionary`
   case bug) end to end. (Verified locally 2026-06-10: env arrives as
   `lidarr_eventtype=Test`, the wrapper returns 0, Lidarr's test returns 200.)
3. **Mock Lidarr API.** Start a local stub HTTP server returning fixed JSON for
   the endpoints `-sync` calls: `config/metadataprovider` (`writeAudioTags=no`)
   and `config/mediamanagement` (`fileDate=none`, permissions off) so
   `run_preflight` passes; and `trackfile`/`track`/`album/{id}`/`artist/{id}`
   describing one artist/album whose `trackfile.path` values are the
   **realpath** of the generated FLACs. Point `MUSEFS_LIDARR_URL` at the stub.
4. **Content leg — tags.** Run `musefs-lidarr-sync` with a constructed
   `AlbumDownload` env (`Lidarr_Album_Id`, `Lidarr_AddedTrackPaths` = the FLAC
   realpaths, which must equal the stub's `trackfile.path` — `match_track_file`
   compares both sides via `realpath_key()`). It queries the stub and writes
   tags to the store.
5. **Content leg — symlink.** Run `musefs-lidarr-import` with
   `Lidarr_SourcePath`/`Lidarr_DestinationPath` set to a generated FLAC and a
   target path; assert the symlink is created.
6. **Serve + assert.** Mount the store with the real binary
   (`musefs mount <mountpoint> --db <db>`) and assert: store tags match the
   stub metadata; backing audio bytes unchanged (sha256 before/after); the
   served mount carries the tags; and — to defend against vacuous passes —
   **at least the seeded track count carry an `artist` tag** (a path-namespace
   mismatch would skip every track and must fail loud, not pass green).

### Enforcement and triggers

- **Required gate in `release-python.yml`:** add a `lidarr-smoke` job that
  `uses:` the reusable workflow, and add it to the `publish` job's `needs`
  (today `needs: [test-python-musefs, test-beets, test-lidarr, test-picard]`,
  `release-python.yml:138`). A Python package release cannot publish without it
  green — the non-forgettable mechanism #224 requires.
- **PR coverage:** the mock-API smoke (`lidarr-smoke.yml`) runs on PRs touching
  `contrib/lidarr/**` or the musefs binary, so the integration stays
  continuously green rather than firing only at release time.
- **Release gate:** the **full real-instance e2e** (`lidarr-e2e.yml`) is the
  job in `publish`'s `needs` — a Python release cannot publish without it green.
- **Download-client path: now covered** (no longer a manual gap) — the full e2e
  drives a real `NewDownload` import → `OnReleaseImport`; see the post-design
  update under *Out of scope*.

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
- **Component 2:** the pure helpers (env builder, ffprobe-tag parse, byte
  equality, the mock-API responses) are unit-tested. The full `lidarr-smoke.yml`
  job needs Docker + `/dev/fuse` and its acceptance evidence is a green
  `workflow_dispatch` run demonstrating all step-6 assertions; it cannot be
  proven on a dev box without those. Because the content leg runs against a
  local mock (not `api.lidarr.audio`), the gate is deterministic and network-free.
- **Component 3:** docs; verified by review against the actual `release.yml`
  DAG and the #162 steps. No automated test.

## Sequencing within the stream

Three PRs. Components 1 and 2 both append steps to the same `python-musefs` job
in `ci.yml` (unit-test steps), so they are **not** fully independent: **merge
Component 1 first**, then Component 2 re-anchors its test step after Component
1's. Otherwise their file surfaces are disjoint. Component 3 (docs) documents
both and owns all `CONTRIBUTING.md` edits, so it is finalized after 1 and 2
settle, though it can be drafted in parallel.

## Decisions resolved during review

- **CI-green gate:** run `ci-ok` + `coverage-ok` on the tag (add the tags
  trigger to `coverage.yml`) and have `gate` poll-and-wait, selecting the
  latest-completed run per name and failing closed only on a completed
  non-success or timeout. Dissolves the tag-on-main question.
- **Publish-loop idempotency:** skip crates already resolvable from the index,
  reusing the #163 probe, so whole-workflow re-runs are safe (closes #222 on
  re-run).
- **Lidarr smoke shape:** a real Lidarr proves only the **Test-event exec path**
  (it cannot deterministically import synthetic files — its MB matcher rejects
  them); the **content** assertions (`-sync` tags, `-import` symlink, served
  tags, bytes unchanged) run against a **local mock Lidarr API**. Deterministic
  and network-free; the tradeoff is no real-Lidarr REST schema-drift coverage.
  Supersedes the earlier "API-driven seed with a live `api.lidarr.audio` fetch"
  decision (RescanArtist could not create trackfiles for synthetic tones).
- **Binary roles (correction):** `musefs-lidarr-sync` queries the API and writes
  tags; `musefs-lidarr-import` only makes the symlink and requires
  `Lidarr_SourcePath`/`Lidarr_DestinationPath`. An earlier draft conflated them.
- **CONTRIBUTING.md ownership:** all edits (new Rust section + the `:365` Lidarr
  gate line) live in Component 3's PR.
