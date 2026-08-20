<!--
Thanks for contributing. The checklist below is short on purpose: it covers
the steps that are easy to forget and that silently break something later.
Everything in it is explained in the contributing guide:
https://sohex.github.io/musefs/contributing/setup.html
-->

## What and why

<!-- What this changes, and the problem it solves. Link the issue: Closes #NNN -->

## Checklist

- [ ] The **pre-commit hook ran** on every commit (no `--no-verify`): `cargo
      fmt`, `cargo clippy --all-targets -- -D warnings`, the full workspace
      test suite, `shellcheck`/`yamllint`, and `ruff`.
      ([Build & test](https://sohex.github.io/musefs/contributing/setup.html#build--test))
- [ ] If the **format-layer API changed**: `cargo +nightly fuzz build` passes.
      `fuzz/` is outside the workspace, so `cargo test` cannot catch a break
      there.
      ([Coverage-guided fuzzing](https://sohex.github.io/musefs/contributing/testing.html#coverage-guided-fuzzing))
- [ ] If the **`musefs-db` schema changed**: the Python mirror was regenerated
      with `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py` and
      re-vendored to the plugins.
      ([Python plugins](https://sohex.github.io/musefs/contributing/plugins.html))
- [ ] **Changelog entry** added under `## [Unreleased]` in `CHANGELOG.md` for
      anything user-visible (`contrib/` has its own changelog).
- [ ] **Docs updated** under `docs/src/` if behaviour, flags, or the store
      contract changed.
- [ ] Commit subjects follow **conventional commits** (`fix(cli): ...`,
      `feat(format): ...`, `ci: ...`), and the body says what changed and why.
      ([Conventions](https://sohex.github.io/musefs/contributing/conventions.html#code-conventions))

## The invariant

- [ ] This change does not copy or modify original audio bytes. A served file
      is still generated metadata spliced in front of positioned reads of the
      untouched backing file.

<!--
Adding a format? It has its own path through the guide — the synthesis trait,
the segment layout, and the test tiers a new format must clear:
https://sohex.github.io/musefs/contributing/conventions.html#adding-a-format
-->
