# Architecture Review Design

## Purpose

Review the musefs architecture through its central invariant: original audio
bytes are never modified, copied, or rewritten. The review should evaluate
whether the current module boundaries, data contracts, cache behavior, and test
surface make that invariant easy to preserve as the project grows.

The primary output is an architecture report, not code changes.

## Scope

The review follows the invariant through the full system:

1. Backing files are scanned into SQLite by `musefs-core`.
2. External tools, especially `contrib/beets`, write desired metadata and art
   into the SQLite contract.
3. `musefs-db` stores tracks, tags, art, version counters, and change triggers.
4. `musefs-format` parses source formats and synthesizes metadata/header
   layouts without owning backing audio bytes.
5. `musefs-core` maps DB rows to virtual paths, resolves synthesized files,
   reads byte ranges, and refreshes caches.
6. `musefs-fuse` exposes the view while preserving read-only behavior,
   concurrency guarantees, stable inodes, and cache invalidation.
7. `musefs-cli` remains a thin entrypoint for scan and mount workflows.

The beets plugin is included as a destination on the route because it is an
external writer into the SQLite contract. The review should check whether its
mapping and lifecycle behavior align with the same invariants that runtime reads
depend on.

## Review Method

Use an invariant-led pass as the backbone, then inspect hotspots where the
invariant crosses complex boundaries:

- SQLite schema, triggers, and version semantics.
- Format parsing and synthesis modules, especially MP4 and Ogg.
- `RegionLayout` and segment ownership semantics.
- Reader assembly, lazy art streaming, and Ogg page patching.
- Virtual tree rebuilds, inode stability, and refresh/cache invalidation.
- FUSE dispatch, worker behavior, and mount option boundaries.
- CLI and beets plugin integration points.

For each area, identify architectural strengths, design risks, unclear
ownership, coupling, module-size pressure, test gaps, and documentation drift.

## Output

Produce a prioritized architecture review with:

- Findings ordered by severity or long-term risk.
- File and line references where useful.
- Clear distinction between correctness risks, maintainability risks, and
  optional refactors.
- Concrete recommendations sized for future implementation work.
- Existing strengths worth preserving.
- Test or documentation gaps that would reduce future regression risk.

The report should avoid speculative rewrites. Recommendations should stay close
to the current layered crate design and the documented SQLite contract.

## Non-Goals

- Do not implement refactors during the review.
- Do not change schema, parser behavior, cache behavior, or FUSE behavior.
- Do not run real FUSE mount tests unless explicitly requested later.
- Do not expand the product scope beyond the current read-only architecture.
