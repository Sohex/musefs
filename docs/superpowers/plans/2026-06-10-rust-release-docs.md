# Rust v* Release Procedure Docs Implementation Plan (#223 + #162)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a "Releasing the Rust crates and binaries" checklist to `CONTRIBUTING.md` covering the version bump, the ordered release graph, index-propagation retries, yank-only rollback, and post-release verification — and replace the prose-only Lidarr release-gate line with a pointer to the automated smoke.

**Architecture:** Documentation only. The new section mirrors the structure and tone of the existing "Releasing the Python packages" section and documents (does not re-explain) the `release.yml` graph that Component 1 builds. This PR owns **all** `CONTRIBUTING.md` edits in the stream — both the new Rust section and the replaced Lidarr gate line — so the two touch points don't collide across PRs.

**Tech Stack:** Markdown.

This plan is Component 3 of the release-process hardening spec:
`docs/superpowers/specs/2026-06-10-release-process-hardening-design.md`.

> **Dependency:** the documented behavior must match the merged Components 1 and
> 2. If those have not merged yet, draft this now but reconcile the exact job
> names / behavior before finalizing (Task 4).

---

## File Structure

- Modify `CONTRIBUTING.md` — add the Rust release section after the existing Python release section (currently ends ~line 415, before `## PRs & commits` at line 417).
- Modify `CONTRIBUTING.md:365` — replace the prose Lidarr release-gate bullet with a pointer to `lidarr-smoke.yml`.

---

## Task 1: Add the "Releasing the Rust crates and binaries" section

**Files:**
- Modify: `CONTRIBUTING.md` (insert before the `## PRs & commits` heading at line 417)

- [ ] **Step 1: Read the surrounding structure**

Run: `grep -n '^## ' CONTRIBUTING.md`
Confirm the "## Releasing the Python packages" heading and the next heading ("## PRs & commits") so the new section lands between them.

- [ ] **Step 2: Insert the new section**

Immediately before the `## PRs & commits` line, insert:

```markdown
## Releasing the Rust crates and binaries

The Rust workspace publishes to crates.io and ships prebuilt cross-compiled
binaries on a `v*` tag, decoupled from the Python `py-v*` flow. `release.yml`
runs one ordered graph — `gate → build → smoke → publish → release-assets` —
and is the source of truth; this checklist is the human side.

**Pre-flight.**

1. Working tree clean, on the commit you intend to release.
2. Confirm `main` is green (CI + coverage). The tag push triggers a fresh
   `ci.yml` and `coverage.yml` run, and the release `gate` job **waits for
   `ci-ok` and `coverage-ok` to be green on the tagged commit** before anything
   builds or publishes — a red tree blocks the release automatically.
3. `CARGO_REGISTRY_TOKEN` is present in repo secrets.

**Version bump (do this in one commit before tagging).**

1. Pick the new version `X.Y.Z`.
2. Bump the workspace `version` in `Cargo.toml`.
3. Bump every internal `musefs-*` path-dependency constraint that pins the old
   version (e.g. `musefs-db = { version = "X.Y.Z", path = "..." }`) — a stale
   internal floor fails the publish.
4. Promote the `## [Unreleased]` section of `CHANGELOG.md` to
   `## [X.Y.Z] - <date>`.
5. Dry-run package each crate: `cargo package -p <crate> --locked` for each of
   `musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs`. This
   catches packaging errors but **not** the cross-crate index-propagation
   problem (it resolves siblings via path deps); that is handled in-workflow
   (next section).
6. Commit, e.g. `git commit -am "release: vX.Y.Z"`.

**Tag and push.**

```bash
git tag vX.Y.Z
git push origin HEAD --tags
```

The tag push starts both CI and `release.yml`. The `gate` job blocks publishing
until `ci-ok` + `coverage-ok` are green on the tagged tree (45-minute timeout,
covering the full matrix including the FreeBSD VM e2e).

**What `release.yml` does.**

1. `gate` — verifies the tag matches the workspace version and waits for the
   required CI checks to pass on the tagged commit (fails closed on a failed
   check or timeout).
2. `build` — cross-compiles the four target binaries.
3. `smoke` — runs the binary smoke on each target (host + Alpine).
4. `publish` — publishes crates in dependency order. For each crate it **skips**
   the publish if `name@version` already resolves from the crates.io index, then
   **waits** for that version to appear before publishing the next dependent
   crate (index-propagation; #163). The skip makes a whole-workflow re-run after
   a partial failure safe.
5. `release-assets` — creates/updates the GitHub Release and uploads the binary
   tarballs + checksums (only after crates publishing succeeds).

**Retry / rollback.**

- crates.io is **yank-only** — a published version cannot be un-published.
- A partial failure (e.g. crate 3 of 6 published, then a transient error) is
  recovered by **re-running the workflow**: the publish loop skips the crates
  already in the index and resumes, then runs `release-assets`. No manual
  cleanup of the published crates is needed.
- GitHub asset upload is idempotent (`gh release upload --clobber`), so re-runs
  re-upload safely.

**Post-release verification.**

1. `cargo install musefs` (or `cargo install musefs --version X.Y.Z`) from a
   clean machine/container.
2. Download a release tarball and verify its checksum:
   `sha256sum -c musefs-X.Y.Z-<triple>.tar.gz.sha256`.
3. Confirm all four target tarballs + `.sha256` files are attached to the
   GitHub Release.

**Lidarr gate at a v1.0.0 milestone.** The Lidarr real-instance smoke
(`lidarr-smoke.yml`) gates the Python `py-v*` release, not this Rust flow. When
a v1.0.0 milestone bundles both, ensure the Python release (and therefore its
Lidarr smoke gate) is also run.
```

- [ ] **Step 3: Verify the file still renders as valid Markdown**

Run: `grep -n '^## Releasing the Rust' CONTRIBUTING.md && grep -n '^## PRs & commits' CONTRIBUTING.md`
Expected: the new heading appears immediately before `## PRs & commits` (the Rust heading's line number is smaller and close to it).

- [ ] **Step 4: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs(contributing): add the Rust v* release procedure (#223, #162)"
```

---

## Task 2: Replace the prose Lidarr release-gate line

**Files:**
- Modify: `CONTRIBUTING.md:365`

The current bullet describes an unenforced convention. Point it at the automated gate Component 2 adds.

- [ ] **Step 1: Replace the bullet**

Find the bullet (currently at `CONTRIBUTING.md:365`):

```markdown
- The Lidarr real-instance smoke test is a release gate, not a default CI job.
  It verifies Lidarr accepts script-created symlink destinations and emits the
  expected Custom Script event.
```

Replace it with:

```markdown
- The Lidarr smoke is automated as a release gate in
  `.github/workflows/lidarr-smoke.yml`: a real Lidarr container proves the
  Custom Script exec path (its Test event), and the content leg
  (`musefs-lidarr-sync` tag-writes, `musefs-lidarr-import` symlink, served-mount
  tags, unchanged backing bytes) runs against a local mock Lidarr API so it is
  deterministic and network-free. It runs on PRs touching the Lidarr surface and
  **gates the Python `py-v*` publish**. The download-client `AlbumImportedEvent`
  path remains a documented manual gap (it only fires for `NewDownload`
  imports); see `docs/superpowers/specs/2026-06-07-lidarr-smoke-checklist.md`.
```

- [ ] **Step 2: Verify**

Run: `grep -n "lidarr-smoke.yml" CONTRIBUTING.md`
Expected: one match in the replaced bullet.

- [ ] **Step 3: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs(contributing): point the Lidarr release-gate note at the automated smoke (#224)"
```

---

## Task 3: Reconcile against the merged workflows

- [ ] **Step 1: Check job and check names match reality**

If Components 1 and 2 have merged, confirm the docs use the real names:

Run: `grep -n "gate:\|publish:\|release-assets:\|ci-ok\|coverage-ok" .github/workflows/release.yml .github/workflows/ci.yml .github/workflows/coverage.yml`
Confirm the section's references (`gate`, `build`, `smoke`, `publish`,
`release-assets`, `ci-ok`, `coverage-ok`, 45-minute gate timeout, 10-minute
index wait) match the merged workflows. Fix any drift in `CONTRIBUTING.md`.

- [ ] **Step 2: Check the lidarr-smoke pointer resolves**

Run: `test -f .github/workflows/lidarr-smoke.yml && echo "workflow exists" || echo "MISSING — reconcile after Component 2 merges"`
If missing because Component 2 has not merged, leave the doc as-is (it forward-references the file) but note it in the PR description.

- [ ] **Step 3: Final read-through**

Read the new section top-to-bottom once. Confirm: no placeholders, the version
list matches `release.yml`'s publish order
(`musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs`), and the
yank-only/idempotent-re-run guidance matches Component 1's publish loop.

- [ ] **Step 4: Commit any reconciliation fixes**

```bash
git add CONTRIBUTING.md
git commit -m "docs(contributing): reconcile Rust release section with merged workflows"
```
(Skip if Step 1–3 found nothing to change.)
