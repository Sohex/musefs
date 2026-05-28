# GitHub Issue Cleanup Plan Index

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement these plans task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the current GitHub issue backlog through eight small,
dependency-ordered PRs.

**Architecture:** The source design is
`docs/superpowers/specs/2026-05-28-github-issue-cleanup-design.md`. Each PR has
its own implementation plan so review scope stays narrow and later workflow
hardening can be rebased over earlier workflow additions.

**Tech Stack:** Rust 2021, Cargo workspace, Python 3, pytest, Ruff, GitHub
Actions, SQLite, cargo-llvm-cov, Codecov.

---

Implement in this order:

1. `github-cleanup/pr-01-coverage-baseline.md`
2. `github-cleanup/pr-02-core-db-read-safety.md`
3. `github-cleanup/pr-03-refresh-invalidation-observability.md`
4. `github-cleanup/pr-04-ogg-hardening-cache-accounting.md`
5. `github-cleanup/pr-05-layout-mp4-contracts.md`
6. `github-cleanup/pr-06-interop-db-contract.md`
7. `github-cleanup/pr-07-beets-python-quality.md`
8. `github-cleanup/pr-08-ci-dev-hardening.md`

Before starting a later PR, rebase it on top of all earlier merged cleanup PRs.
This matters especially for PR 8, which must harden workflows introduced by PR 1
and PR 7.
