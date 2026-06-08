# Edition 2024 — improvement candidates (PR 2 seed)

**Date:** 2026-06-08
**Status:** Inventory only — nothing applied. Input to PR 2's brainstorm.
**Context:** Produced during the edition 2024 bump (PR 1,
`docs/superpowers/plans/2026-06-08-edition-2024-bump.md`).

## Headline finding

**The bump itself already applied every mechanically-beneficial let-chain
collapse.** Under edition 2024, `clippy::collapsible_if` (a default-`warn`
lint, escalated to error by the `-D warnings` gate) suggests merging a nested
`if let P = e { if c { … } }` into `if let P = e && c { … }`. Because the
workspace clippy gate denies warnings, those collapses were *forced* during the
bump commit (`ba8f744`) — 9 sites across 5 files, not optional cleanup:

| File | Line | Collapsed chain |
| --- | --- | --- |
| `musefs-format/src/mp3.rs` | 113, 651 | `if let Some(tail) = tail && … && &tail[0..3] == b"TAG"` (bool chain); POPM `if let Some(nul) … && let Some((&rating, counter)) …` |
| `musefs-core/src/facade.rs` | 747, 758, 780, 785 | refresh-diff `inode_of_track` guards merged into the surrounding conditions (`&& let` at 748/758/781/786) |
| `musefs-fuse/src/lib.rs` | 226 | `… && let Err(inval_err) = n.inval_inode(…)` |
| `musefs-fuse/src/platform/passthrough.rs` | 59 | `… && let Some(pfd) = core.passthrough_fd(fh)` |
| `musefs-latencyfs/src/lib.rs` | 419, 585 | `… && let Ok(s) = statvfs(&p)`; `… && let Err(e) = OpenOptions::new()…` |

So the "quick look for what edition 2024 improves" that motivated this work was,
for let-chains, **answered by the migration itself**. PR 2's residual scope is
small — possibly empty.

## Residual let-chain candidates

Of the 69 remaining `if let` sites in `src/` (non-test), the nested ones that
clippy did *not* collapse were each checked. None is a clean, behavior-
preserving let-chain win:

- **Intervening `let` binding** (can't appear mid-chain in a 2024 let-chain).
  Example `musefs-core/src/scan.rs:837`:
  ```rust
  if let Some(&(size, mtime, id, format)) = existing.get(&key) {
      let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
      if size == meta.len() && mtime == mtime_secs(&meta) && !needs_backfill { … }
  }
  ```
  `needs_backfill` is derived from the destructured fields and reused in the
  inner test; a let-chain cannot bind it, so collapsing would require inlining
  the expression twice. Not a win.

- **Inner `if` has an `else`** → collapsing changes semantics (the `else` would
  also catch the outer `None`). Example `musefs-format/src/ogg/mod.rs:483`:
  ```rust
  if let Some(PayloadChunk::Bytes(b)) = bp.first_mut() {
      if i + 1 == n { b[0] |= 0x80; } else { b[0] &= 0x7F; }
  }
  ```
  A `&& i + 1 == n { … } else { … }` chain would run the `else` when
  `bp.first_mut()` is `None`. Incorrect — leave as-is.

- **`else if let` fallback chains** (e.g. `musefs-format/src/mp3.rs:575–579`,
  the comment/lyrics/text fallback) are disjunctive alternatives, not nested
  conjunctions — let-chains do not apply.

- **`match` arms with guards** (e.g. `musefs-core/src/refresh_diff.rs:49,76`,
  `musefs-core/src/tree.rs:633`, `musefs-format/src/tagmap.rs:176,184,200`) are
  already the idiomatic form; rewriting them as `if let … &&` guards would be a
  regression in clarity, not an improvement.

## Other edition-2024 ergonomics

- **Precise capturing (`+ use<>`):** the workspace has exactly one `-> impl`
  return (`musefs-format/tests/proptest_flac.rs:6`), which captures no
  lifetimes; `cargo fix --edition` produced no `use<>` bounds. Nothing to adopt.
- **Never-type fallback / `let_and_return`:** one `let_and_return` in
  `musefs-db/src/tracks.rs` was force-fixed by clippy during the bump (the 2024
  tail-expression temporary-scope change made the transform drop-order-safe).
  No further opportunities observed.
- **`gen` blocks / generators:** still unstable; not available on stable.

## Recommendation for PR 2

PR 2 may not be worth opening as a code change. The let-chain value the edition
unlocked was auto-applied under the clippy gate during PR 1, and the residual
candidates are either semantically unsafe to collapse, blocked by intervening
bindings, or already idiomatic. If PR 2 proceeds, it should be a deliberate
*readability* pass (a human deciding a specific `else if let` ladder or match
reads better restructured), not a mechanical sweep — and each change weighed
individually, since none is gate-forced.
