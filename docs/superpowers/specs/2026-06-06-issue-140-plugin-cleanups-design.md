# Issue #140: plugin minor cleanups (beets/Picard)

**Issue:** https://github.com/Sohex/musefs/issues/140
**Date:** 2026-06-06
**Scope:** contrib plugins only (`contrib/picard`, `contrib/beets`). No Rust
changes, no `musefs_common` library changes.

Five independent micro-cleanups. Each section stands alone.

## 1. `PLUGIN_API_VERSIONS` declares the floor only

**File:** `contrib/picard/musefs/__init__.py`

Replace the hand-enumerated `["2.0", "2.1", …, "2.13"]` list with `["2.0"]`.

Rationale: Picard's plugin loader (`pluginmanager.py::_compatible_api_versions`)
takes the **set intersection** of the plugin's declared versions with
`picard.api_versions`, and every Picard 2.x release retains the full
back-catalog (`"2.0"` through current) in `api_versions`. Declaring the floor
the plugin actually requires therefore loads on every Picard 2.x with zero
per-release maintenance. The existing comment already documents that all
required APIs (`BaseAction`, `register_*_action`, `OptionsPage`,
`register_options_page`, `config.TextOption`/`BoolOption`, `thread.run_task`,
`iterfiles`, `metadata.images`, `is_front_image`) exist since Picard 2.0.0.

Picard 3.x uses a new plugin system and will not load 2.x-style plugins
regardless of this list, so enumerating newer versions buys nothing.

Changes:
- `PLUGIN_API_VERSIONS = ["2.0"]`.
- Keep the API-inventory comment (it already states the 2.0 floor); the added
  line explains only the intersection semantics — why declaring the floor
  alone suffices — without restating the floor.
- Add a test asserting `PLUGIN_API_VERSIONS == ["2.0"]` so a future
  hand-extension gets flagged. The constant sits *outside* the `if _PICARD:`
  block, so the assert must NOT be `importorskip("picard")`-gated — placed
  where it runs under the default pytest invocation too, catching drift on
  hosts without Picard.

## 2. `dry_run` unused from Picard — no code change

`sync_files`/`sync_one` accept `dry_run`; the Picard UI never passes it. The
surface is **not dead**: the beets plugin's `--dry-run` flag drives it
(`beetsplug/musefs.py`). Adding a dry-run preview to Picard's UI is real scope
with no demand. Resolution: informational; close the bullet with a note,
change nothing.

## 3. beets `_query_from_args` returns a list on both paths

**File:** `contrib/beets/beetsplug/musefs.py` (`MusefsPlugin._query_from_args`)

The `sync`-verb branch returns `args[1:]` (a tuple slice when the caller hands
a tuple); the fallthrough returns `list(args)`. Change the first branch to
`return list(args[1:])`.

Tests: `contrib/beets/tests/test_plugin.py` already covers both paths but only
with list inputs. Extend it to feed a tuple through the `sync`-verb branch —
`_query_from_args(("sync", "artist:Band"))` — asserting the result equals the
expected list and `type(result) is list`, exercising the actual bug path.

## 4. Picard `_resolved_files` logs dropped duplicates

**File:** `contrib/picard/musefs/__init__.py` (`_resolved_files`)

Replace the bare `seen.setdefault(realpath_key(...), f)` with an explicit
membership check. Log only when the key is already present **and** the stored
value is a different object — i.e. the suppression rule is identity-based,
`seen[key] is not f` (the same `File` re-yielded by overlapping selections is
not interesting):

```python
log.debug("musefs: duplicate file for %s: %r dropped in favor of %r", ...)
```

`log` is already imported and used in this scope. Behavior is otherwise
unchanged: first file wins.

Tests: **net-new** — no existing test exercises `_resolved_files` (the only
mention in the suite is a `conftest.py` docstring). Add a dedicated test file
mirroring `test_callback_flow.py`'s structure: `importorskip("picard")`-gated,
since `_resolved_files` lives inside the `if _PICARD:` block and is only
importable with real Picard (so it runs only under the real-Picard harness).
Picard's `log` is not stdlib `logging`, so pytest's `caplog` captures nothing;
use the suite's established pattern instead —
`monkeypatch.setattr(musefs.log, "debug", recorder)` as in
`test_callback_flow.py:37`. Two cases:
- two distinct `File`s sharing one realpath key → duplicate dropped (first
  wins) **and** one debug record emitted;
- the same `File` object yielded twice → dropped, **no** debug record.

## 5. beets `sniff_mime` receives the real path, not the lossy key

**File:** `contrib/beets/beetsplug/_core.py` (`_read_album_art`)

Compute `real = os.path.realpath(artpath)` once; keep using it for `open()`
(already the case) and pass `os.fsdecode(real)` — instead of the
U+FFFD-normalized `key` — as `sniff_mime`'s path argument.

Honesty note: this is **behavior-equivalent today**. U+FFFD replacement cannot
create or remove `.` characters, and a genuinely non-UTF-8 extension misses
the ASCII-keyed `_EXT_MIME` table under any decoding, so the issue's "defeats
the extension fallback" claim does not hold for the current table. The change
is hygiene: don't feed a display-normalized DB key into a path-semantic
helper (same principle as the existing comment about opening the raw realpath
rather than the lossy form). No new test asserts the unobservable distinction;
existing `sniff_mime`/art tests cover the unchanged behavior.

## Verification

- `cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .`
- beets: `.venv` pytest run (system Python is externally managed).
- Picard: default pytest run **and** the real-Picard run
  (`/usr/bin/python3` + `PYTHONPATH` to `/usr/lib/picard`), which the default
  run silently skips.
- No vendored-copy regeneration needed: `musefs_common` is untouched.
