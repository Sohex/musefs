# GitHub Issue Cleanup Design

## Context

The repository has nineteen open GitHub issues as of 2026-05-28. Most were
created from the architecture review in
`docs/superpowers/reviews/2026-05-28-architecture-review.md`; the rest cover
CI, Python tooling, coverage, and local development gates.

The cleanup should optimize for lowest review risk rather than the fewest pull
requests. Each pull request should have a narrow ownership boundary, close its
listed issues, and include focused verification for the behavior it changes.

## PR Sequence

Work will proceed in seven dependency-ordered pull requests:

1. `core-db-read-safety`: close #4 and #5.
2. `refresh-invalidation-observability`: close #6, #7, and #8.
3. `ogg-hardening-cache-accounting`: close #9, #10, and #16.
4. `layout-mp4-contracts`: close #15 and #17.
5. `interop-db-contract`: close #11 and #14.
6. `beets-python-quality`: close #12, #13, and #19.
7. `ci-dev-hardening`: close #2, #18, #20, and #21.

Later PRs may broaden CI coverage, but each behavior-changing PR must include
its own targeted tests. No later PR should be responsible for proving the
correctness of behavior introduced earlier.

## PR 1: Core DB Read Safety

This PR fixes two high-risk correctness bugs in the core read path.

`DbPool` must stop reusing a thread-local SQLite connection across different DB
paths. The preferred design is to key the thread-local connection by canonical
database path while preserving the existing per-thread read-only connection
model.

Open handles must validate the metadata of the file descriptor that was actually
opened before the handle is cached. The check should compare size and mtime
against the resolved backing-file contract, so a path replacement between
resolve and open does not serve bytes from a file that does not match the cached
layout.

Verification should include a same-thread two-pool regression test and a focused
open-handle metadata validation test that proves a mismatched opened descriptor
is rejected before it can be cached.

## PR 2: Refresh Invalidation Observability

This PR owns refresh retry semantics, keep-cache invalidation correctness, and
visible FUSE failure reporting.

Failed refresh attempts must not consume the debounce window. The successful
data-version stamp should remain committed only after a successful rebuild.

Keep-cache invalidation must handle path-changing retags. When a changed track
moves to a new rendered path, the stale old inode must be reported for kernel
cache invalidation. The implementation may also report the new inode where that
is useful, but it must not rely only on the rebuilt tree's current inode lookup.

The FUSE adapter should remain thin, but refresh and `inval_inode` failures
should be surfaced through minimal logging so operators are not left with silent
staleness.

Verification should cover failed-refresh retry behavior and path-changing retag
invalidation behavior without requiring real FUSE failure injection.

## PR 3: Ogg Hardening And Cache Accounting

This PR keeps Ogg-specific hardening in one reviewable unit.

Ogg cover-art synthesis must guard the full `METADATA_BLOCK_PICTURE`
VorbisComment value length before casting to `u32`. The guard must include the
key, picture prefix, and base64 image length.

Resident Ogg page indexes must be accounted for in cache budgeting, or a
separate bounded policy must be implemented and tested. The chosen policy should
make page-dense Ogg files visible to memory limits instead of leaving the page
index outside the advertised cache budget.

The Ogg invariant must be documented and tested precisely: original packet
payload bytes are preserved, while Ogg page sequence numbers and CRCs may be
patched intentionally.

## PR 4: Layout And MP4 Contracts

This PR clarifies format-layer contracts without changing MP4 behavior.

`RegionLayout` should gain a lightweight validation path at synthesis
boundaries. The simple segment representation should remain intact; validation
should make producer invariants easier to test and debug without forcing a broad
type-system rewrite.

MP4/M4A synthesis should continue silently using the first cover-art input. The
README must document this as an intentional current limitation: MP4/M4A embeds
only the first cover image when multiple images are available. Tests should lock
that behavior so it remains deliberate.

## PR 5: Interop And DB Contract

This PR strengthens public contract tests for external writers and independent
readers.

The SQLite schema documentation should clarify which structural fields are
scanner-owned and which fields external writers may safely update. External
tools such as Beets should remain on the scanner path for structural rows unless
a future design explicitly expands the contract.

The independent-reader interop test should verify synthesized outputs preserve
the source audio payload, not only that synthesized tags are readable. The
manifest or fixtures should include enough per-format audio payload metadata to
compare source and synthesized audio bytes while respecting the Ogg page-header
patching model.

This PR should not broaden Beets CI; that belongs to PR 6.

## PR 6: Beets Python Quality

This PR handles Python integration and Beets contract coverage as one unit.

Beets pruning should be constrained to the sync/scan scope instead of deleting
every missing backing path in the database. The implementation should add
regression coverage for a scoped Beets sync with an unrelated missing row and
prove that unrelated row is preserved.

CI should exercise the Beets SQLite writer contract narrowly enough to stay
reliable: sync, retag, prune, and art writes. Ruff should be configured for all
repository Python code under `contrib/beets/` and `tests/interop/`, with both
lint and format-check commands documented.

This PR may adjust Python style and plugin tests. It should avoid unrelated Rust
workflow changes.

## PR 7: CI And Development Hardening

This PR finishes repository workflow hardening.

All GitHub Actions `uses:` references in the workflows should be pinned to full
commit SHAs, and every `actions/checkout` step should set
`persist-credentials: false`.

Coverage reporting should use `cargo-llvm-cov` and upload to Codecov. Required
Codecov setup, tokens, and local limitations should be documented. Coverage
should not require ignored FUSE tests unless they are configured as a separate
explicit job.

The CLI should gain a pure test seam for converting parsed mount flags into
`MountConfig` and `FuseConfig` without mounting. The local pre-commit hook should
enforce the normal local passing test gate and document what it runs.

## Verification Gates

PRs 1 through 5 should run at least:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

PRs touching format, Ogg, or interop behavior should also run focused checks
such as:

```bash
cargo test -p musefs-format --features fuzzing
cargo test -p musefs-core --test interop_emit
```

PR 6 should run the Python lint/test gate introduced in that PR. Rust checks are
required if Rust files change.

PR 7 should verify workflow YAML statically where possible, run the local hook
command directly, and run CLI tests for the mount-config seam. Codecov upload
may require GitHub/Codecov repository setup and should be documented if it
cannot be fully verified locally.

## Guardrails

- Keep `musefs-fuse` and `musefs-cli` thin.
- Preserve original audio bytes exactly, with the Ogg exception stated as
  payload bytes preserved and page headers patched.
- Keep external writers away from scanner-owned structural fields unless a
  future explicit design expands that contract.
- Avoid unrelated refactors in these cleanup PRs.
- Use `Closes #N` in every PR body for the exact issues handled by that PR.
