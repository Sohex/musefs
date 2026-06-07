# Lidarr Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `contrib/lidarr` Python integration that lets Lidarr import into a symlink or hardlink placeholder tree, then sync Lidarr metadata into the musefs SQLite store without letting Lidarr copy or rewrite backing audio bytes.

**Architecture:** The new package follows the existing contrib pattern: `python-musefs` remains the shared DB contract, and `contrib/lidarr` owns Lidarr-specific environment parsing, link creation, API access, metadata mapping, CLI entry points, docs, and tests. The default import path creates symlinks and exits `0` so Lidarr skips its internal `HardLinkOrCopy`; the sync path scans affected paths and writes `musefs_common.Record`s into the store.

**Tech Stack:** Python 3.9+, stdlib `argparse`/`urllib.request`, `pytest`, `ruff`, `python-musefs`, Lidarr API v1, existing musefs SQLite schema and `musefs scan`.

---

## File Structure

Create these files:

- `contrib/lidarr/pyproject.toml` — package metadata, script entry points, pytest config.
- `contrib/lidarr/requirements.txt` — local test/development requirements.
- `contrib/lidarr/ruff.toml` — Ruff config, mirroring the other contrib packages.
- `contrib/lidarr/src/musefs_lidarr/__init__.py` — public package version.
- `contrib/lidarr/src/musefs_lidarr/errors.py` — user-facing exception classes.
- `contrib/lidarr/src/musefs_lidarr/import_link.py` — import-script env parsing and symlink/hardlink creation.
- `contrib/lidarr/src/musefs_lidarr/events.py` — Custom Script event parsing.
- `contrib/lidarr/src/musefs_lidarr/api.py` — Lidarr API client, endpoint wrappers, redaction, config preflight.
- `contrib/lidarr/src/musefs_lidarr/mapping.py` — path-to-track matching and Lidarr payload to musefs tag pairs.
- `contrib/lidarr/src/musefs_lidarr/sync.py` — scan, DB transaction, event sync, rename pruning behavior.
- `contrib/lidarr/src/musefs_lidarr/cli_import.py` — `musefs-lidarr-import` entry point.
- `contrib/lidarr/src/musefs_lidarr/cli_sync.py` — `musefs-lidarr-sync` entry point.
- `contrib/lidarr/tests/conftest.py` — SQLite fixture and sample Lidarr payloads.
- `contrib/lidarr/tests/test_import_link.py` — import script tests.
- `contrib/lidarr/tests/test_events.py` — event parsing tests.
- `contrib/lidarr/tests/test_api.py` — API client, redaction, and preflight tests.
- `contrib/lidarr/tests/test_mapping.py` — path matching and tag mapping tests.
- `contrib/lidarr/tests/test_sync.py` — DB sync and rename-mode tests.
- `contrib/lidarr/tests/test_cli.py` — CLI behavior and exit code tests.
- `contrib/lidarr/tests/test_path_gate.py` — opt-in real `musefs` path matching gate.
- `contrib/lidarr/README.md` — Lidarr setup and safety docs.

Modify these files:

- `README.md` — mention Lidarr integration with beets/Picard.
- `ARCHITECTURE.md` — add Lidarr to contrib ecosystem and describe placeholder tree vs musefs mount.
- `CONTRIBUTING.md` — add Lidarr test commands and gotchas.
- `contrib/python-musefs/README.md` — add Lidarr as a consumer.
- `.github/workflows/ci.yml` — add a `lidarr` CI job and include it in `ci-ok`.
- `CHANGELOG.md` — add an unreleased entry if the current changelog has an unreleased section; otherwise add a concise top entry.

---

### Task 1: Package Scaffold

**Files:**
- Create: `contrib/lidarr/pyproject.toml`
- Create: `contrib/lidarr/requirements.txt`
- Create: `contrib/lidarr/ruff.toml`
- Create: `contrib/lidarr/src/musefs_lidarr/__init__.py`
- Create: `contrib/lidarr/src/musefs_lidarr/errors.py`
- Create: `contrib/lidarr/tests/test_smoke.py`

- [ ] **Step 1: Write the failing package smoke test**

Create `contrib/lidarr/tests/test_smoke.py`:

```python
from musefs_lidarr import __version__
from musefs_lidarr.errors import MusefsLidarrError


def test_package_imports():
    assert __version__ == "0.1.0"
    assert str(MusefsLidarrError("problem")) == "problem"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `rtk python -m pytest contrib/lidarr/tests/test_smoke.py -v`

Expected: FAIL with `ModuleNotFoundError: No module named 'musefs_lidarr'`.

- [ ] **Step 3: Create the package files**

Create `contrib/lidarr/pyproject.toml`:

```toml
[build-system]
requires = ["setuptools>=61"]
build-backend = "setuptools.build_meta"

[project]
name = "lidarr-musefs"
version = "0.1.0"
description = "Sync Lidarr metadata into the musefs SQLite store"
requires-python = ">=3.9"
dependencies = ["python-musefs>=0.1.0"]

[project.optional-dependencies]
test = ["pytest>=7"]

[project.scripts]
musefs-lidarr-import = "musefs_lidarr.cli_import:main"
musefs-lidarr-sync = "musefs_lidarr.cli_sync:main"

[tool.setuptools.packages.find]
where = ["src"]

[tool.pytest.ini_options]
testpaths = ["tests"]
pythonpath = ["src"]
markers = [
    "musefs_bin: tests that shell out to the real `musefs` Rust binary (opt-in)",
]
addopts = "-m 'not musefs_bin'"
```

Create `contrib/lidarr/requirements.txt`:

```text
pytest>=7
```

Create `contrib/lidarr/ruff.toml`:

```toml
line-length = 99
target-version = "py39"
```

Create `contrib/lidarr/src/musefs_lidarr/__init__.py`:

```python
"""Lidarr integration for syncing metadata into musefs."""

__version__ = "0.1.0"
```

Create `contrib/lidarr/src/musefs_lidarr/errors.py`:

```python
class MusefsLidarrError(Exception):
    """Base class for user-facing Lidarr integration failures."""


class ConfigError(MusefsLidarrError):
    """Configuration or environment variable failure."""


class ImportLinkError(MusefsLidarrError):
    """Import-script link creation failure."""


class LidarrApiError(MusefsLidarrError):
    """Lidarr API failure."""


class MappingError(MusefsLidarrError):
    """Ambiguous or unsupported Lidarr metadata mapping."""
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_smoke.py -v`

Expected: PASS.

- [ ] **Step 5: Run lint**

Run: `rtk ruff check contrib/lidarr/ && rtk ruff format --check contrib/lidarr/`

Expected: PASS. If `ruff` is unavailable, install the package test dependencies inside a venv before rerunning.

- [ ] **Step 6: Commit**

```bash
rtk git add contrib/lidarr/pyproject.toml contrib/lidarr/requirements.txt contrib/lidarr/ruff.toml contrib/lidarr/src/musefs_lidarr/__init__.py contrib/lidarr/src/musefs_lidarr/errors.py contrib/lidarr/tests/test_smoke.py
rtk git commit -m "feat(lidarr): scaffold plugin package"
```

---

### Task 2: Import Script Link Creation

**Files:**
- Create: `contrib/lidarr/src/musefs_lidarr/import_link.py`
- Modify: `contrib/lidarr/src/musefs_lidarr/errors.py`
- Test: `contrib/lidarr/tests/test_import_link.py`

- [ ] **Step 1: Write failing tests for env parsing and link mode**

Create `contrib/lidarr/tests/test_import_link.py` with these initial tests:

```python
import os

import pytest

from musefs_lidarr.errors import ConfigError
from musefs_lidarr.import_link import LinkMode, parse_import_env, parse_link_mode


def test_parse_link_mode_defaults_to_symlink():
    assert parse_link_mode({}) == LinkMode.SYMLINK


def test_parse_link_mode_accepts_hardlink():
    assert parse_link_mode({"MUSEFS_LIDARR_LINK_MODE": "hardlink"}) == LinkMode.HARDLINK


def test_parse_link_mode_rejects_unknown_value():
    with pytest.raises(ConfigError, match="MUSEFS_LIDARR_LINK_MODE"):
        parse_link_mode({"MUSEFS_LIDARR_LINK_MODE": "copy"})


def test_parse_import_env_reads_lidarr_paths(tmp_path):
    src = tmp_path / "source.flac"
    dst = tmp_path / "artist" / "album" / "dest.flac"
    src.write_bytes(b"audio")

    env = {
        "Lidarr_SourcePath": os.fsdecode(src),
        "Lidarr_DestinationPath": os.fsdecode(dst),
    }

    parsed = parse_import_env(env)

    assert parsed.source == src
    assert parsed.destination == dst
    assert parsed.mode == LinkMode.SYMLINK


def test_parse_import_env_missing_paths_fails():
    with pytest.raises(ConfigError, match="Lidarr_SourcePath"):
        parse_import_env({})
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_import_link.py -v`

Expected: FAIL with `ModuleNotFoundError` or `ImportError` for `musefs_lidarr.import_link`.

- [ ] **Step 3: Implement env parsing and enum**

Create `contrib/lidarr/src/musefs_lidarr/import_link.py` with:

```python
from __future__ import annotations

import os
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

from .errors import ConfigError, ImportLinkError


class LinkMode(Enum):
    SYMLINK = "symlink"
    HARDLINK = "hardlink"


@dataclass(frozen=True)
class ImportRequest:
    source: Path
    destination: Path
    mode: LinkMode


def parse_link_mode(environ: dict[str, str]) -> LinkMode:
    raw = environ.get("MUSEFS_LIDARR_LINK_MODE", "symlink").strip().lower()
    try:
        return LinkMode(raw)
    except ValueError as exc:
        raise ConfigError(
            "MUSEFS_LIDARR_LINK_MODE must be 'symlink' or 'hardlink', "
            f"got {raw!r}"
        ) from exc


def _required_path(environ: dict[str, str], name: str) -> Path:
    value = environ.get(name)
    if not value:
        raise ConfigError(f"{name} is required")
    return Path(value)


def parse_import_env(environ: dict[str, str] | None = None) -> ImportRequest:
    env = os.environ if environ is None else environ
    return ImportRequest(
        source=_required_path(env, "Lidarr_SourcePath"),
        destination=_required_path(env, "Lidarr_DestinationPath"),
        mode=parse_link_mode(env),
    )
```

- [ ] **Step 4: Run tests to verify parsing passes**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_import_link.py -v`

Expected: PASS for the parsing tests.

- [ ] **Step 5: Add failing tests for symlink creation and idempotency**

Append to `contrib/lidarr/tests/test_import_link.py`:

```python
from musefs_lidarr.errors import ImportLinkError
from musefs_lidarr.import_link import ensure_link


def test_ensure_link_creates_symlink(tmp_path):
    src = tmp_path / "downloads" / "song.flac"
    dst = tmp_path / "library" / "Artist" / "song.flac"
    src.parent.mkdir()
    src.write_bytes(b"audio")

    ensure_link(src, dst, LinkMode.SYMLINK)

    assert dst.is_symlink()
    assert dst.readlink() == src
    assert dst.resolve() == src


def test_ensure_link_symlink_is_idempotent(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")
    dst.parent.mkdir()
    dst.symlink_to(src)

    ensure_link(src, dst, LinkMode.SYMLINK)

    assert dst.is_symlink()
    assert dst.readlink() == src


def test_ensure_link_refuses_conflicting_destination(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")
    dst.parent.mkdir()
    dst.write_bytes(b"other")

    with pytest.raises(ImportLinkError, match="destination already exists"):
        ensure_link(src, dst, LinkMode.SYMLINK)


def test_ensure_link_missing_source_fails(tmp_path):
    with pytest.raises(ImportLinkError, match="source does not exist"):
        ensure_link(tmp_path / "missing.flac", tmp_path / "library" / "song.flac", LinkMode.SYMLINK)
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_import_link.py -v`

Expected: FAIL with `ImportError: cannot import name 'ensure_link'`.

- [ ] **Step 7: Implement symlink creation**

Append these functions to `import_link.py`:

```python
def _same_symlink(source: Path, destination: Path) -> bool:
    return destination.is_symlink() and destination.readlink() == source


def _same_inode(source: Path, destination: Path) -> bool:
    try:
        return source.stat().st_ino == destination.stat().st_ino and source.stat().st_dev == destination.stat().st_dev
    except FileNotFoundError:
        return False


def ensure_link(source: Path, destination: Path, mode: LinkMode) -> None:
    if not source.exists():
        raise ImportLinkError(f"source does not exist: {source}")

    destination.parent.mkdir(parents=True, exist_ok=True)

    if destination.exists() or destination.is_symlink():
        if mode is LinkMode.SYMLINK and _same_symlink(source, destination):
            return
        if mode is LinkMode.HARDLINK and _same_inode(source, destination):
            return
        raise ImportLinkError(f"destination already exists and does not match source: {destination}")

    try:
        if mode is LinkMode.SYMLINK:
            destination.symlink_to(source)
        else:
            os.link(source, destination)
    except OSError as exc:
        raise ImportLinkError(f"failed to create {mode.value}: {source} -> {destination}: {exc}") from exc
```

Run `rtk ruff format contrib/lidarr/src/musefs_lidarr/import_link.py contrib/lidarr/tests/test_import_link.py` after adding the code. This formatting command is allowed as a mechanical rewrite.

- [ ] **Step 8: Run tests to verify symlink behavior passes**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_import_link.py -v`

Expected: PASS.

- [ ] **Step 9: Add hardlink tests**

Append:

```python
def test_ensure_link_creates_hardlink(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")

    ensure_link(src, dst, LinkMode.HARDLINK)

    assert not dst.is_symlink()
    assert os.stat(src).st_ino == os.stat(dst).st_ino
    assert os.stat(src).st_dev == os.stat(dst).st_dev


def test_ensure_link_hardlink_is_idempotent(tmp_path):
    src = tmp_path / "song.flac"
    dst = tmp_path / "library" / "song.flac"
    src.write_bytes(b"audio")
    dst.parent.mkdir()
    os.link(src, dst)

    ensure_link(src, dst, LinkMode.HARDLINK)

    assert os.stat(src).st_ino == os.stat(dst).st_ino
```

- [ ] **Step 10: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_import_link.py -v`

Expected: PASS.

- [ ] **Step 11: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/import_link.py contrib/lidarr/tests/test_import_link.py
rtk git commit -m "feat(lidarr): add import link creation"
```

---

### Task 3: Custom Script Event Parsing

**Files:**
- Create: `contrib/lidarr/src/musefs_lidarr/events.py`
- Test: `contrib/lidarr/tests/test_events.py`

- [ ] **Step 1: Write failing tests**

Create `contrib/lidarr/tests/test_events.py`:

```python
from musefs_lidarr.events import EventType, parse_event, split_paths


def test_split_paths_handles_empty_value():
    assert split_paths("") == []


def test_split_paths_splits_pipe_separated_paths():
    assert split_paths("/a.flac|/b.flac") == ["/a.flac", "/b.flac"]


def test_parse_test_event():
    event = parse_event({"Lidarr_EventType": "Test"})

    assert event.event_type == EventType.TEST
    assert event.paths == []
    assert event.previous_paths == []


def test_parse_album_download_event():
    event = parse_event({
        "Lidarr_EventType": "AlbumDownload",
        "Lidarr_Artist_Id": "12",
        "Lidarr_Album_Id": "34",
        "Lidarr_AddedTrackPaths": "/music/a.flac|/music/b.flac",
    })

    assert event.event_type == EventType.ALBUM_DOWNLOAD
    assert event.artist_id == 12
    assert event.album_id == 34
    assert event.paths == ["/music/a.flac", "/music/b.flac"]


def test_parse_rename_event():
    event = parse_event({
        "Lidarr_EventType": "Rename",
        "Lidarr_Artist_Id": "12",
        "Lidarr_TrackFile_Paths": "/new/a.flac|/new/b.flac",
        "Lidarr_TrackFile_PreviousPaths": "/old/a.flac|/old/b.flac",
    })

    assert event.event_type == EventType.RENAME
    assert event.artist_id == 12
    assert event.paths == ["/new/a.flac", "/new/b.flac"]
    assert event.previous_paths == ["/old/a.flac", "/old/b.flac"]


def test_parse_unknown_event_is_unsupported():
    event = parse_event({"Lidarr_EventType": "Grab"})

    assert event.event_type == EventType.UNSUPPORTED
    assert event.raw_type == "Grab"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_events.py -v`

Expected: FAIL with `ModuleNotFoundError` for `musefs_lidarr.events`.

- [ ] **Step 3: Implement event parser**

Create `contrib/lidarr/src/musefs_lidarr/events.py`:

```python
from __future__ import annotations

import os
from dataclasses import dataclass, field
from enum import Enum


class EventType(Enum):
    TEST = "Test"
    ALBUM_DOWNLOAD = "AlbumDownload"
    RENAME = "Rename"
    TRACK_RETAG = "TrackRetag"
    UNSUPPORTED = "Unsupported"


@dataclass(frozen=True)
class LidarrEvent:
    event_type: EventType
    raw_type: str
    paths: list[str] = field(default_factory=list)
    previous_paths: list[str] = field(default_factory=list)
    artist_id: int | None = None
    album_id: int | None = None


def split_paths(value: str | None) -> list[str]:
    if not value:
        return []
    return [part for part in value.split("|") if part]


def _int_or_none(value: str | None) -> int | None:
    if not value:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def parse_event(environ: dict[str, str] | None = None) -> LidarrEvent:
    env = os.environ if environ is None else environ
    raw = env.get("Lidarr_EventType", "")

    if raw == EventType.TEST.value:
        event_type = EventType.TEST
    elif raw == EventType.ALBUM_DOWNLOAD.value:
        event_type = EventType.ALBUM_DOWNLOAD
    elif raw == EventType.RENAME.value:
        event_type = EventType.RENAME
    elif raw == EventType.TRACK_RETAG.value:
        event_type = EventType.TRACK_RETAG
    else:
        event_type = EventType.UNSUPPORTED

    paths = []
    previous_paths = []
    if event_type is EventType.ALBUM_DOWNLOAD:
        paths = split_paths(env.get("Lidarr_AddedTrackPaths"))
    elif event_type is EventType.RENAME:
        paths = split_paths(env.get("Lidarr_TrackFile_Paths"))
        previous_paths = split_paths(env.get("Lidarr_TrackFile_PreviousPaths"))

    return LidarrEvent(
        event_type=event_type,
        raw_type=raw,
        paths=paths,
        previous_paths=previous_paths,
        artist_id=_int_or_none(env.get("Lidarr_Artist_Id")),
        album_id=_int_or_none(env.get("Lidarr_Album_Id")),
    )
```

- [ ] **Step 4: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_events.py -v`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/events.py contrib/lidarr/tests/test_events.py
rtk git commit -m "feat(lidarr): parse custom script events"
```

---

### Task 4: Lidarr API Client, Redaction, and Preflight

**Files:**
- Create: `contrib/lidarr/src/musefs_lidarr/api.py`
- Test: `contrib/lidarr/tests/test_api.py`

- [ ] **Step 1: Write failing API tests**

Create `contrib/lidarr/tests/test_api.py`:

```python
import json
from urllib.error import HTTPError

import pytest

from musefs_lidarr.api import (
    LidarrClient,
    LidarrConfig,
    PreflightResult,
    check_safe_settings,
    redacted,
)
from musefs_lidarr.errors import ConfigError, LidarrApiError


class FakeResponse:
    def __init__(self, payload):
        self.payload = json.dumps(payload).encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def read(self):
        return self.payload


def test_redacted_masks_secret():
    assert redacted("abc123") == "<redacted>"
    assert redacted("") == "<missing>"


def test_client_builds_url_and_api_key_header():
    calls = []

    def opener(request, timeout):
        calls.append((request.full_url, dict(request.header_items()), timeout))
        return FakeResponse({"ok": True})

    client = LidarrClient(LidarrConfig(url="http://lidarr.local/", api_key="secret"), opener=opener)

    assert client.get_json("/api/v1/trackfile", {"artistId": 7}) == {"ok": True}

    assert calls[0][0] == "http://lidarr.local/api/v1/trackfile?artistId=7"
    assert calls[0][1]["X-api-key"] == "secret"
    assert calls[0][2] == 15


def test_client_wraps_http_error_without_key():
    def opener(request, timeout):
        raise HTTPError(request.full_url, 401, "Unauthorized", hdrs=None, fp=None)

    client = LidarrClient(LidarrConfig(url="http://lidarr.local", api_key="secret"), opener=opener)

    with pytest.raises(LidarrApiError) as exc:
        client.get_json("/api/v1/trackfile", {"artistId": 7})

    message = str(exc.value)
    assert "401" in message
    assert "secret" not in message
    assert "<redacted>" in message


def test_check_safe_settings_passes():
    result = check_safe_settings(
        metadata={"writeAudioTags": "no"},
        media_management={
            "fileDate": "none",
            "setPermissionsLinux": False,
        },
    )

    assert result == PreflightResult(ok=True, errors=[])


def test_check_safe_settings_reports_all_unsafe_values():
    result = check_safe_settings(
        metadata={"writeAudioTags": "allFiles"},
        media_management={
            "fileDate": "albumReleaseDate",
            "setPermissionsLinux": True,
        },
    )

    assert result.ok is False
    assert result.errors == [
        "writeAudioTags must be no, got allFiles",
        "fileDate must be none, got albumReleaseDate",
        "setPermissionsLinux must be false",
    ]


def test_config_requires_url_and_key_together():
    with pytest.raises(ConfigError, match="MUSEFS_LIDARR_URL"):
        LidarrConfig.from_env({"MUSEFS_LIDARR_API_KEY": "secret"})
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_api.py -v`

Expected: FAIL with `ModuleNotFoundError` for `musefs_lidarr.api`.

- [ ] **Step 3: Implement API client and preflight helpers**

Create `contrib/lidarr/src/musefs_lidarr/api.py`:

```python
from __future__ import annotations

import json
import os
from dataclasses import dataclass
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from .errors import ConfigError, LidarrApiError


def redacted(value: str | None) -> str:
    return "<redacted>" if value else "<missing>"


@dataclass(frozen=True)
class LidarrConfig:
    url: str | None = None
    api_key: str | None = None

    @classmethod
    def from_env(cls, environ: dict[str, str] | None = None) -> "LidarrConfig":
        env = os.environ if environ is None else environ
        url = env.get("MUSEFS_LIDARR_URL") or None
        api_key = env.get("MUSEFS_LIDARR_API_KEY") or None
        if bool(url) != bool(api_key):
            raise ConfigError("MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY must be set together")
        return cls(url=url, api_key=api_key)

    @property
    def enabled(self) -> bool:
        return bool(self.url and self.api_key)


@dataclass(frozen=True)
class PreflightResult:
    ok: bool
    errors: list[str]


class LidarrClient:
    def __init__(self, config: LidarrConfig, *, opener=urlopen, timeout: int = 15):
        if not config.url or not config.api_key:
            raise ConfigError("Lidarr API configuration is required")
        self._base = config.url.rstrip("/")
        self._api_key = config.api_key
        self._opener = opener
        self._timeout = timeout

    def get_json(self, path: str, params: dict[str, object] | None = None):
        query = ""
        if params:
            clean = {k: v for k, v in params.items() if v is not None}
            if clean:
                query = "?" + urlencode(clean, doseq=True)
        url = f"{self._base}{path}{query}"
        request = Request(url, headers={"X-Api-Key": self._api_key})
        try:
            with self._opener(request, timeout=self._timeout) as response:
                return json.loads(response.read().decode("utf-8"))
        except HTTPError as exc:
            raise LidarrApiError(
                f"Lidarr API request failed with HTTP {exc.code}; api_key={redacted(self._api_key)}"
            ) from exc
        except URLError as exc:
            raise LidarrApiError(f"Lidarr API request failed: {exc.reason}") from exc
        except json.JSONDecodeError as exc:
            raise LidarrApiError("Lidarr API returned invalid JSON") from exc

    def media_management_config(self):
        return self.get_json("/api/v1/config/mediamanagement")

    def metadata_provider_config(self):
        return self.get_json("/api/v1/config/metadataprovider")

    def track_files(self, *, artist_id=None, album_id=None, track_file_ids=None):
        params = {}
        if artist_id is not None:
            params["artistId"] = artist_id
        if album_id is not None:
            params["albumId"] = [album_id]
        if track_file_ids:
            params["trackFileIds"] = list(track_file_ids)
        return self.get_json("/api/v1/trackfile", params)

    def tracks(self, *, artist_id=None, album_id=None, track_ids=None):
        params = {}
        if artist_id is not None:
            params["artistId"] = artist_id
        if album_id is not None:
            params["albumId"] = album_id
        if track_ids:
            params["trackIds"] = list(track_ids)
        return self.get_json("/api/v1/track", params)

    def album(self, album_id: int):
        return self.get_json(f"/api/v1/album/{album_id}")

    def artist(self, artist_id: int):
        return self.get_json(f"/api/v1/artist/{artist_id}")


def _lower(value) -> str:
    return str(value).strip().lower()


def check_safe_settings(metadata: dict, media_management: dict) -> PreflightResult:
    errors = []
    if _lower(metadata.get("writeAudioTags")) != "no":
        errors.append(f"writeAudioTags must be no, got {metadata.get('writeAudioTags')}")
    if _lower(media_management.get("fileDate")) != "none":
        errors.append(f"fileDate must be none, got {media_management.get('fileDate')}")
    if bool(media_management.get("setPermissionsLinux")):
        errors.append("setPermissionsLinux must be false")
    return PreflightResult(ok=not errors, errors=errors)


def run_preflight(client: LidarrClient) -> PreflightResult:
    return check_safe_settings(
        metadata=client.metadata_provider_config(),
        media_management=client.media_management_config(),
    )
```

- [ ] **Step 4: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_api.py -v`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/api.py contrib/lidarr/tests/test_api.py
rtk git commit -m "feat(lidarr): add API client and preflight"
```

---

### Task 5: Metadata Mapping and Path Matching

**Files:**
- Create: `contrib/lidarr/src/musefs_lidarr/mapping.py`
- Create: `contrib/lidarr/tests/conftest.py`
- Test: `contrib/lidarr/tests/test_mapping.py`

- [ ] **Step 1: Create shared test fixtures**

Create `contrib/lidarr/tests/conftest.py`:

```python
import sqlite3
import time

import pytest
from musefs_common import connect as musefs_connect
from musefs_common.schema import SCHEMA_SQL


@pytest.fixture
def sample_artist():
    return {
        "id": 10,
        "artistName": "Boards of Canada",
        "foreignArtistId": "artist-mbid",
        "genres": ["Electronic", "IDM"],
    }


@pytest.fixture
def sample_album(sample_artist):
    return {
        "id": 20,
        "title": "Music Has the Right to Children",
        "foreignAlbumId": "release-group-mbid",
        "releaseDate": "1998-04-20T00:00:00Z",
        "genres": ["Electronic"],
        "artist": sample_artist,
    }


@pytest.fixture
def sample_track_file(tmp_path):
    path = tmp_path / "library" / "01 - Wildlife Analysis.flac"
    path.parent.mkdir()
    path.write_bytes(b"audio")
    return {
        "id": 30,
        "artistId": 10,
        "albumId": 20,
        "path": str(path),
        "releaseGroup": "Skam",
    }


@pytest.fixture
def sample_track(sample_track_file):
    return {
        "id": 40,
        "artistId": 10,
        "albumId": 20,
        "trackFileId": sample_track_file["id"],
        "foreignTrackId": "track-mbid",
        "foreignRecordingId": "recording-mbid",
        "trackNumber": "1",
        "mediumNumber": 1,
        "title": "Wildlife Analysis",
    }


@pytest.fixture
def db_path(tmp_path):
    path = tmp_path / "musefs.db"
    conn = sqlite3.connect(str(path))
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    conn.close()
    return str(path)


def insert_track(conn, backing_path, fmt="flac"):
    now = int(time.time())
    cur = conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
        "backing_size, backing_mtime, updated_at) VALUES (?, ?, 0, 0, 0, 0, ?)",
        (backing_path, fmt, now),
    )
    return cur.lastrowid


@pytest.fixture
def make_track(db_path):
    def _make(backing_path, fmt="flac"):
        conn = musefs_connect(db_path)
        try:
            tid = insert_track(conn, backing_path, fmt)
            conn.commit()
            return tid
        finally:
            conn.close()

    return _make
```

- [ ] **Step 2: Write failing mapping tests**

Create `contrib/lidarr/tests/test_mapping.py`:

```python
import pytest
from musefs_common import realpath_key

from musefs_lidarr.errors import MappingError
from musefs_lidarr.mapping import build_pairs, match_track_file, records_for_paths


def test_build_pairs_maps_core_tags(sample_artist, sample_album, sample_track):
    pairs = build_pairs(track=sample_track, album=sample_album, artist=sample_artist)

    assert ("title", "Wildlife Analysis") in pairs
    assert ("artist", "Boards of Canada") in pairs
    assert ("albumartist", "Boards of Canada") in pairs
    assert ("album", "Music Has the Right to Children") in pairs
    assert ("tracknumber", "1") in pairs
    assert ("discnumber", "1") in pairs
    assert ("date", "1998-04-20") in pairs
    assert ("musicbrainz_artistid", "artist-mbid") in pairs
    assert ("musicbrainz_albumid", "release-group-mbid") in pairs
    assert ("musicbrainz_trackid", "track-mbid") in pairs
    assert ("musicbrainz_releasetrackid", "recording-mbid") in pairs
    assert pairs.count(("genre", "Electronic")) == 1
    assert ("genre", "IDM") in pairs


def test_match_track_file_by_realpath(sample_track_file):
    key = realpath_key(sample_track_file["path"])

    assert match_track_file(key, [sample_track_file]) == sample_track_file


def test_match_track_file_zero_match_returns_none(sample_track_file, tmp_path):
    key = realpath_key(tmp_path / "other.flac")

    assert match_track_file(key, [sample_track_file]) is None


def test_match_track_file_multiple_matches_fails(sample_track_file):
    key = realpath_key(sample_track_file["path"])
    duplicate = dict(sample_track_file, id=31)

    with pytest.raises(MappingError, match="multiple Lidarr track files"):
        match_track_file(key, [sample_track_file, duplicate])


def test_records_for_paths_builds_record(sample_artist, sample_album, sample_track, sample_track_file):
    records, skipped = records_for_paths(
        paths=[sample_track_file["path"]],
        track_files=[sample_track_file],
        tracks=[sample_track],
        album=sample_album,
        artist=sample_artist,
    )

    assert skipped == []
    assert len(records) == 1
    assert records[0].key == realpath_key(sample_track_file["path"])
    assert ("title", "Wildlife Analysis") in records[0].pairs
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_mapping.py -v`

Expected: FAIL with `ModuleNotFoundError` for `musefs_lidarr.mapping`.

- [ ] **Step 4: Implement mapping**

Create `contrib/lidarr/src/musefs_lidarr/mapping.py`:

```python
from __future__ import annotations

from collections import defaultdict

from musefs_common import Record, realpath_key

from .errors import MappingError


def _text(value) -> str | None:
    if value is None:
        return None
    out = str(value).strip()
    return out or None


def _date(value) -> str | None:
    text = _text(value)
    if not text:
        return None
    return text[:10] if len(text) >= 10 else text


def _append(pairs: list[tuple[str, str]], key: str, value) -> None:
    text = _text(value)
    if text:
        pairs.append((key, text))


def build_pairs(*, track: dict, album: dict, artist: dict) -> list[tuple[str, str]]:
    pairs: list[tuple[str, str]] = []
    artist_name = artist.get("artistName") or artist.get("name")
    _append(pairs, "title", track.get("title"))
    _append(pairs, "artist", artist_name)
    _append(pairs, "albumartist", artist_name)
    _append(pairs, "album", album.get("title"))
    _append(pairs, "tracknumber", track.get("trackNumber") or track.get("absoluteTrackNumber"))
    _append(pairs, "discnumber", track.get("mediumNumber"))
    _append(pairs, "date", _date(album.get("releaseDate")))
    _append(pairs, "musicbrainz_artistid", artist.get("foreignArtistId") or artist.get("mbId"))
    _append(pairs, "musicbrainz_albumid", album.get("foreignAlbumId"))
    _append(pairs, "musicbrainz_trackid", track.get("foreignTrackId"))
    _append(pairs, "musicbrainz_releasetrackid", track.get("foreignRecordingId"))

    seen_genres = set()
    for genre in list(album.get("genres") or []) + list(artist.get("genres") or []):
        text = _text(genre)
        if text and text not in seen_genres:
            seen_genres.add(text)
            pairs.append(("genre", text))
    return pairs


def match_track_file(path_key: str, track_files: list[dict]) -> dict | None:
    matches = [tf for tf in track_files if realpath_key(tf["path"]) == path_key]
    if len(matches) > 1:
        ids = ", ".join(str(tf.get("id")) for tf in matches)
        raise MappingError(f"multiple Lidarr track files match {path_key}: {ids}")
    return matches[0] if matches else None


def _tracks_by_file(tracks: list[dict]) -> dict[int, list[dict]]:
    grouped: dict[int, list[dict]] = defaultdict(list)
    for track in tracks:
        grouped[int(track["trackFileId"])].append(track)
    return grouped


def records_for_paths(
    *,
    paths: list[str],
    track_files: list[dict],
    tracks: list[dict],
    album: dict,
    artist: dict,
) -> tuple[list[Record], list[str]]:
    tracks_by_file = _tracks_by_file(tracks)
    records = []
    skipped = []
    for path in paths:
        key = realpath_key(path)
        track_file = match_track_file(key, track_files)
        if track_file is None:
            skipped.append(path)
            continue
        linked = tracks_by_file.get(int(track_file["id"]), [])
        if not linked:
            skipped.append(path)
            continue
        pairs = []
        for track in linked:
            pairs.extend(build_pairs(track=track, album=album, artist=artist))
        records.append(Record(key=key, pairs=pairs, art=None))
    return records, skipped
```

- [ ] **Step 5: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_mapping.py -v`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/mapping.py contrib/lidarr/tests/conftest.py contrib/lidarr/tests/test_mapping.py
rtk git commit -m "feat(lidarr): map Lidarr metadata to musefs records"
```

---

### Task 6: Sync Workflow and DB Writes

**Files:**
- Create: `contrib/lidarr/src/musefs_lidarr/sync.py`
- Test: `contrib/lidarr/tests/test_sync.py`

- [ ] **Step 1: Write failing sync tests**

Create `contrib/lidarr/tests/test_sync.py`:

```python
from musefs_common import connect, realpath_key

from musefs_lidarr.events import EventType, LidarrEvent
from musefs_lidarr.import_link import LinkMode
from musefs_lidarr.sync import SyncConfig, sync_records, sync_rename_prune


def test_sync_records_writes_tags(db_path, make_track, sample_track_file, sample_track, sample_album, sample_artist):
    key = realpath_key(sample_track_file["path"])
    make_track(key)
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )
    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK, autoscan=False)

    stats = sync_records(
        config=config,
        event=event,
        track_files=[sample_track_file],
        tracks=[sample_track],
        album=sample_album,
        artist=sample_artist,
    )

    assert stats.synced == 1
    conn = connect(db_path)
    try:
        rows = conn.execute(
            "SELECT key, value FROM tags ORDER BY key, ordinal"
        ).fetchall()
    finally:
        conn.close()
    assert ("title", "Wildlife Analysis") in rows
    assert ("genre", "Electronic") in rows


def test_sync_records_counts_missing_row_as_skipped(db_path, sample_track_file, sample_track, sample_album, sample_artist):
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )
    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK, autoscan=False)

    stats = sync_records(
        config=config,
        event=event,
        track_files=[sample_track_file],
        tracks=[sample_track],
        album=sample_album,
        artist=sample_artist,
    )

    assert stats.synced == 0
    assert stats.skipped == 1


def test_symlink_rename_does_not_prune_previous_placeholder(db_path, make_track, sample_track_file, tmp_path):
    key = realpath_key(sample_track_file["path"])
    make_track(key)
    old_placeholder = tmp_path / "old.flac"
    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK, autoscan=False)

    pruned = sync_rename_prune(config=config, previous_paths=[str(old_placeholder)])

    assert pruned == 0


def test_hardlink_rename_prunes_previous_missing_path(db_path, make_track, tmp_path):
    old_path = tmp_path / "old.flac"
    make_track(realpath_key(old_path))
    config = SyncConfig(db_path=db_path, link_mode=LinkMode.HARDLINK, autoscan=False)

    pruned = sync_rename_prune(config=config, previous_paths=[str(old_path)])

    assert pruned == 1
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_sync.py -v`

Expected: FAIL with `ModuleNotFoundError` for `musefs_lidarr.sync`.

- [ ] **Step 3: Implement sync helpers**

Create `contrib/lidarr/src/musefs_lidarr/sync.py`:

```python
from __future__ import annotations

from dataclasses import dataclass

from musefs_common import (
    SyncStats,
    check_schema_version,
    connect,
    prune_missing,
    realpath_key,
    sync_files,
)

from .events import LidarrEvent
from .import_link import LinkMode
from .mapping import records_for_paths


@dataclass(frozen=True)
class SyncConfig:
    db_path: str
    link_mode: LinkMode
    autoscan: bool = True
    musefs_bin: str = "musefs"


def sync_records(
    *,
    config: SyncConfig,
    event: LidarrEvent,
    track_files: list[dict],
    tracks: list[dict],
    album: dict,
    artist: dict,
) -> SyncStats:
    records, skipped_paths = records_for_paths(
        paths=event.paths,
        track_files=track_files,
        tracks=tracks,
        album=album,
        artist=artist,
    )
    stats = SyncStats(skipped=len(skipped_paths))
    conn = connect(config.db_path)
    try:
        check_schema_version(conn)
        sync_files(conn, records, stats=stats)
        conn.commit()
        return stats
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


def sync_rename_prune(*, config: SyncConfig, previous_paths: list[str]) -> int:
    if config.link_mode is LinkMode.SYMLINK or not previous_paths:
        return 0

    previous_keys = [realpath_key(path) for path in previous_paths]
    conn = connect(config.db_path)
    try:
        rows = conn.execute(
            "SELECT id FROM tracks WHERE backing_path IN ({})".format(
                ",".join("?" for _ in previous_keys)
            ),
            previous_keys,
        ).fetchall()
        pruned = prune_missing(conn, [row[0] for row in rows])
        conn.commit()
        return pruned
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()
```

- [ ] **Step 4: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_sync.py -v`

Expected: PASS.

- [ ] **Step 5: Add autoscan wrapper tests**

Append to `contrib/lidarr/tests/test_sync.py`:

```python
from musefs_lidarr.sync import scan_if_enabled


def test_scan_if_enabled_skips_when_disabled(tmp_path):
    calls = []
    config = SyncConfig(db_path=str(tmp_path / "m.db"), link_mode=LinkMode.SYMLINK, autoscan=False)

    scan_if_enabled(config=config, paths=["/music/a.flac"], runner=lambda *args, **kwargs: calls.append(args))

    assert calls == []


def test_scan_if_enabled_calls_runner(tmp_path):
    calls = []
    config = SyncConfig(db_path=str(tmp_path / "m.db"), link_mode=LinkMode.SYMLINK, autoscan=True, musefs_bin="musefs-dev")

    scan_if_enabled(config=config, paths=["/music/a.flac"], runner=lambda binary, db_path, targets: calls.append((binary, db_path, targets)))

    assert calls == [("musefs-dev", str(tmp_path / "m.db"), ["/music/a.flac"])]
```

- [ ] **Step 6: Implement autoscan wrapper**

Append to `sync.py`:

```python
from musefs_common import run_scan


def scan_if_enabled(*, config: SyncConfig, paths: list[str], runner=run_scan) -> None:
    if not config.autoscan or not paths:
        return
    runner(config.musefs_bin, config.db_path, paths)
```

- [ ] **Step 7: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_sync.py -v`

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/sync.py contrib/lidarr/tests/test_sync.py
rtk git commit -m "feat(lidarr): sync Lidarr records into musefs"
```

---

### Task 7: CLI Entry Points

**Files:**
- Create: `contrib/lidarr/src/musefs_lidarr/cli_import.py`
- Create: `contrib/lidarr/src/musefs_lidarr/cli_sync.py`
- Test: `contrib/lidarr/tests/test_cli.py`

- [ ] **Step 1: Write failing CLI import tests**

Create `contrib/lidarr/tests/test_cli.py`:

```python
from musefs_lidarr.cli_import import run as run_import
from musefs_lidarr.cli_sync import run as run_sync


def test_import_cli_test_event_exits_zero(capsys):
    rc = run_import({"Lidarr_EventType": "Test"})

    assert rc == 0
    assert "test ok" in capsys.readouterr().out


def test_import_cli_creates_symlink(tmp_path):
    src = tmp_path / "src.flac"
    dst = tmp_path / "library" / "dst.flac"
    src.write_bytes(b"audio")

    rc = run_import({
        "Lidarr_SourcePath": str(src),
        "Lidarr_DestinationPath": str(dst),
    })

    assert rc == 0
    assert dst.is_symlink()


def test_sync_cli_test_event_exits_zero(capsys):
    rc = run_sync([], {"Lidarr_EventType": "Test"})

    assert rc == 0
    assert "test ok" in capsys.readouterr().out
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_cli.py -v`

Expected: FAIL with `ModuleNotFoundError` for `musefs_lidarr.cli_import`.

- [ ] **Step 3: Implement import CLI**

Create `contrib/lidarr/src/musefs_lidarr/cli_import.py`:

```python
from __future__ import annotations

import os
import sys

from .errors import MusefsLidarrError
from .import_link import ensure_link, parse_import_env


def run(environ: dict[str, str] | None = None) -> int:
    env = os.environ if environ is None else environ
    if env.get("Lidarr_EventType") == "Test":
        print("musefs-lidarr-import: test ok")
        return 0
    try:
        request = parse_import_env(env)
        ensure_link(request.source, request.destination, request.mode)
        print(
            "musefs-lidarr-import: "
            f"{request.mode.value} {request.source} -> {request.destination}"
        )
        return 0
    except MusefsLidarrError as exc:
        print(f"musefs-lidarr-import: {exc}", file=sys.stderr)
        return 1


def main() -> int:
    return run()
```

- [ ] **Step 4: Implement sync CLI test behavior**

Create `contrib/lidarr/src/musefs_lidarr/cli_sync.py`:

```python
from __future__ import annotations

import argparse
import os
import sys

from .api import LidarrConfig, LidarrClient, run_preflight
from .errors import ConfigError, LidarrApiError, MusefsLidarrError
from .events import EventType, parse_event


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="musefs-lidarr-sync")
    parser.add_argument("--doctor", action="store_true", help="check Lidarr settings and exit")
    parser.add_argument("--skip-lidarr-preflight", action="store_true")
    return parser


def _doctor(environ: dict[str, str]) -> int:
    config = LidarrConfig.from_env(environ)
    client = LidarrClient(config)
    result = run_preflight(client)
    if result.ok:
        print("musefs-lidarr-sync: doctor ok")
        return 0
    for error in result.errors:
        print(f"musefs-lidarr-sync: unsafe Lidarr setting: {error}", file=sys.stderr)
    return 1


def run(argv: list[str] | None = None, environ: dict[str, str] | None = None) -> int:
    env = os.environ if environ is None else environ
    args = _parser().parse_args([] if argv is None else argv)
    try:
        if args.doctor:
            return _doctor(env)
        event = parse_event(env)
        if event.event_type is EventType.TEST:
            print("musefs-lidarr-sync: test ok")
            return 0
        if event.event_type is EventType.UNSUPPORTED:
            print(f"musefs-lidarr-sync: unsupported event {event.raw_type!r}; skipping")
            return 0
        raise ConfigError("MUSEFS_DB is required for AlbumDownload, Rename, and TrackRetag events")
    except (MusefsLidarrError, LidarrApiError) as exc:
        print(f"musefs-lidarr-sync: {exc}", file=sys.stderr)
        return 1


def main() -> int:
    return run(sys.argv[1:])
```

- [ ] **Step 5: Run current CLI tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_cli.py -v`

Expected: PASS for test/import-script behavior.

- [ ] **Step 6: Add doctor CLI tests**

Append:

```python
def test_sync_cli_unsupported_event_exits_zero(capsys):
    rc = run_sync([], {"Lidarr_EventType": "Grab"})

    assert rc == 0
    assert "unsupported event" in capsys.readouterr().out
```

- [ ] **Step 7: Run CLI tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_cli.py -v`

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/cli_import.py contrib/lidarr/src/musefs_lidarr/cli_sync.py contrib/lidarr/tests/test_cli.py
rtk git commit -m "feat(lidarr): add script entry points"
```

---

### Task 8: Wire API-Backed Sync CLI

**Files:**
- Modify: `contrib/lidarr/src/musefs_lidarr/cli_sync.py`
- Modify: `contrib/lidarr/src/musefs_lidarr/api.py`
- Modify: `contrib/lidarr/src/musefs_lidarr/sync.py`
- Test: `contrib/lidarr/tests/test_cli.py`
- Test: `contrib/lidarr/tests/test_sync.py`

- [ ] **Step 1: Add a test for config parsing**

Append to `contrib/lidarr/tests/test_sync.py`:

```python
from musefs_lidarr.sync import SyncConfig, config_from_env


def test_config_from_env_reads_required_values(tmp_path):
    config = config_from_env({
        "MUSEFS_DB": str(tmp_path / "musefs.db"),
        "MUSEFS_BIN": "musefs-dev",
        "MUSEFS_LIDARR_AUTOSCAN": "0",
        "MUSEFS_LIDARR_LINK_MODE": "hardlink",
    })

    assert config == SyncConfig(
        db_path=str(tmp_path / "musefs.db"),
        link_mode=LinkMode.HARDLINK,
        autoscan=False,
        musefs_bin="musefs-dev",
    )
```

- [ ] **Step 2: Implement config parsing**

Append to `sync.py`:

```python
import os

from .errors import ConfigError
from .import_link import parse_link_mode


def _env_bool(value: str | None, *, default: bool) -> bool:
    if value is None:
        return default
    return value.strip().lower() not in {"0", "false", "no", "off"}


def config_from_env(environ: dict[str, str] | None = None) -> SyncConfig:
    env = os.environ if environ is None else environ
    db_path = env.get("MUSEFS_DB")
    if not db_path:
        raise ConfigError("MUSEFS_DB is required")
    return SyncConfig(
        db_path=db_path,
        link_mode=parse_link_mode(env),
        autoscan=_env_bool(env.get("MUSEFS_LIDARR_AUTOSCAN"), default=True),
        musefs_bin=env.get("MUSEFS_BIN") or "musefs",
    )
```

- [ ] **Step 3: Run sync tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_sync.py -v`

Expected: PASS.

- [ ] **Step 4: Add workflow function test with fake client data**

Append to `contrib/lidarr/tests/test_sync.py`:

```python
from musefs_lidarr.sync import sync_event_with_payloads


def test_sync_event_with_payloads_scans_then_syncs(db_path, make_track, sample_track_file, sample_track, sample_album, sample_artist):
    key = realpath_key(sample_track_file["path"])
    make_track(key)
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )
    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK, autoscan=True, musefs_bin="musefs")
    scan_calls = []

    stats = sync_event_with_payloads(
        config=config,
        event=event,
        track_files=[sample_track_file],
        tracks=[sample_track],
        album=sample_album,
        artist=sample_artist,
        scanner=lambda binary, db_path, targets: scan_calls.append((binary, db_path, targets)),
    )

    assert scan_calls == [("musefs", db_path, [sample_track_file["path"]])]
    assert stats.synced == 1
```

- [ ] **Step 5: Implement workflow function**

Append to `sync.py`:

```python
def sync_event_with_payloads(
    *,
    config: SyncConfig,
    event: LidarrEvent,
    track_files: list[dict],
    tracks: list[dict],
    album: dict,
    artist: dict,
    scanner=run_scan,
) -> SyncStats:
    scan_if_enabled(config=config, paths=event.paths, runner=scanner)
    stats = sync_records(
        config=config,
        event=event,
        track_files=track_files,
        tracks=tracks,
        album=album,
        artist=artist,
    )
    sync_rename_prune(config=config, previous_paths=event.previous_paths)
    return stats
```

- [ ] **Step 6: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_sync.py -v`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/sync.py contrib/lidarr/tests/test_sync.py
rtk git commit -m "feat(lidarr): wire sync workflow"
```

- [ ] **Step 8: Add tests for API payload collection**

Append to `contrib/lidarr/tests/test_sync.py`:

```python
from musefs_lidarr.sync import collect_event_payloads


class FakeLidarrClient:
    def __init__(self, *, track_files, tracks, album, artist):
        self.track_file_calls = []
        self.track_calls = []
        self._track_files = track_files
        self._tracks = tracks
        self._album = album
        self._artist = artist

    def track_files(self, **kwargs):
        self.track_file_calls.append(kwargs)
        return self._track_files

    def tracks(self, **kwargs):
        self.track_calls.append(kwargs)
        return self._tracks

    def album(self, album_id):
        assert album_id == self._album["id"]
        return self._album

    def artist(self, artist_id):
        assert artist_id == self._artist["id"]
        return self._artist


def test_collect_event_payloads_queries_by_album_when_available(
    sample_track_file, sample_track, sample_album, sample_artist
):
    client = FakeLidarrClient(
        track_files=[sample_track_file],
        tracks=[sample_track],
        album=sample_album,
        artist=sample_artist,
    )
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )

    payloads = collect_event_payloads(client=client, event=event)

    assert client.track_file_calls == [{"album_id": 20}]
    assert client.track_calls == [{"album_id": 20}]
    assert payloads.track_files == [sample_track_file]
    assert payloads.tracks == [sample_track]
    assert payloads.album == sample_album
    assert payloads.artist == sample_artist
```

- [ ] **Step 9: Implement payload collection**

Append to `sync.py`:

```python
@dataclass(frozen=True)
class EventPayloads:
    track_files: list[dict]
    tracks: list[dict]
    album: dict
    artist: dict


def collect_event_payloads(*, client, event: LidarrEvent) -> EventPayloads:
    if event.album_id is not None:
        track_files = client.track_files(album_id=event.album_id)
        tracks = client.tracks(album_id=event.album_id)
        album = client.album(event.album_id)
        artist_id = event.artist_id or album["artistId"]
        artist = client.artist(artist_id)
        return EventPayloads(
            track_files=track_files,
            tracks=tracks,
            album=album,
            artist=artist,
        )
    if event.artist_id is not None:
        track_files = client.track_files(artist_id=event.artist_id)
        tracks = client.tracks(artist_id=event.artist_id)
        artist = client.artist(event.artist_id)
        album_id = track_files[0]["albumId"] if track_files else 0
        album = client.album(album_id) if album_id else {}
        return EventPayloads(
            track_files=track_files,
            tracks=tracks,
            album=album,
            artist=artist,
        )
    raise ConfigError("Lidarr event must include Lidarr_Album_Id or Lidarr_Artist_Id")
```

- [ ] **Step 10: Run tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_sync.py -v`

Expected: PASS.

- [ ] **Step 11: Add CLI sync execution test with injected runner**

Append to `contrib/lidarr/tests/test_cli.py`:

```python
def test_sync_cli_requires_db_for_album_download(capsys):
    rc = run_sync([], {
        "Lidarr_EventType": "AlbumDownload",
        "Lidarr_AddedTrackPaths": "/music/a.flac",
    })

    assert rc == 1
    assert "MUSEFS_DB is required" in capsys.readouterr().err
```

- [ ] **Step 12: Run CLI tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_cli.py -v`

Expected: PASS.

- [ ] **Step 13: Commit API-backed collection**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/sync.py contrib/lidarr/tests/test_sync.py contrib/lidarr/tests/test_cli.py
rtk git commit -m "feat(lidarr): collect Lidarr API payloads"
```

- [ ] **Step 14: Add CLI execution test with injected dependencies**

Append to `contrib/lidarr/tests/test_cli.py`:

```python
def test_sync_cli_runs_api_backed_event(tmp_path, capsys, sample_track_file, sample_track, sample_album, sample_artist):
    calls = []

    class FakeClient:
        def track_files(self, **kwargs):
            return [sample_track_file]

        def tracks(self, **kwargs):
            return [sample_track]

        def album(self, album_id):
            return sample_album

        def artist(self, artist_id):
            return sample_artist

        def metadata_provider_config(self):
            return {"writeAudioTags": "no"}

        def media_management_config(self):
            return {"fileDate": "none", "setPermissionsLinux": False}

    def fake_sync(**kwargs):
        calls.append(kwargs)
        class Stats:
            def summary(self):
                return "synced=1 skipped=0 art_linked=0 skipped_art=0"
        return Stats()

    rc = run_sync(
        [],
        {
            "Lidarr_EventType": "AlbumDownload",
            "Lidarr_Artist_Id": "10",
            "Lidarr_Album_Id": "20",
            "Lidarr_AddedTrackPaths": sample_track_file["path"],
            "MUSEFS_DB": str(tmp_path / "musefs.db"),
            "MUSEFS_LIDARR_URL": "http://lidarr.local",
            "MUSEFS_LIDARR_API_KEY": "secret",
        },
        client_factory=lambda config: FakeClient(),
        sync_runner=fake_sync,
    )

    assert rc == 0
    assert calls[0]["track_files"] == [sample_track_file]
    assert calls[0]["tracks"] == [sample_track]
    assert "synced=1" in capsys.readouterr().out
```

- [ ] **Step 15: Update sync CLI to run real events**

Replace `contrib/lidarr/src/musefs_lidarr/cli_sync.py` with:

```python
from __future__ import annotations

import argparse
import os
import sys

from .api import LidarrClient, LidarrConfig, run_preflight
from .errors import ConfigError, LidarrApiError, MusefsLidarrError
from .events import EventType, parse_event
from .sync import collect_event_payloads, config_from_env, sync_event_with_payloads


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="musefs-lidarr-sync")
    parser.add_argument("--doctor", action="store_true", help="check Lidarr settings and exit")
    parser.add_argument("--skip-lidarr-preflight", action="store_true")
    return parser


def _doctor(client) -> int:
    result = run_preflight(client)
    if result.ok:
        print("musefs-lidarr-sync: doctor ok")
        return 0
    for error in result.errors:
        print(f"musefs-lidarr-sync: unsafe Lidarr setting: {error}", file=sys.stderr)
    return 1


def run(
    argv: list[str] | None = None,
    environ: dict[str, str] | None = None,
    *,
    client_factory=LidarrClient,
    sync_runner=sync_event_with_payloads,
) -> int:
    env = os.environ if environ is None else environ
    args = _parser().parse_args([] if argv is None else argv)
    try:
        lidarr_config = LidarrConfig.from_env(env)
        client = client_factory(lidarr_config) if lidarr_config.enabled else None
        if args.doctor:
            if client is None:
                raise ConfigError("MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY are required for doctor")
            return _doctor(client)

        event = parse_event(env)
        if event.event_type is EventType.TEST:
            print("musefs-lidarr-sync: test ok")
            return 0
        if event.event_type is EventType.UNSUPPORTED:
            print(f"musefs-lidarr-sync: unsupported event {event.raw_type!r}; skipping")
            return 0
        if client is None:
            print(
                "musefs-lidarr-sync: warning: Lidarr API settings are absent; "
                "only env-only sync is available and unsafe Lidarr settings cannot be verified",
                file=sys.stderr,
            )
            raise ConfigError("MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY are required for v1 event sync")

        sync_config = config_from_env(env)
        if not args.skip_lidarr_preflight:
            doctor_rc = _doctor(client)
            if doctor_rc != 0:
                return doctor_rc

        payloads = collect_event_payloads(client=client, event=event)
        stats = sync_runner(
            config=sync_config,
            event=event,
            track_files=payloads.track_files,
            tracks=payloads.tracks,
            album=payloads.album,
            artist=payloads.artist,
        )
        print(f"musefs-lidarr-sync: {stats.summary()}")
        return 0
    except (MusefsLidarrError, LidarrApiError) as exc:
        print(f"musefs-lidarr-sync: {exc}", file=sys.stderr)
        return 1


def main() -> int:
    return run(sys.argv[1:])
```

- [ ] **Step 16: Run CLI tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_cli.py -v`

Expected: PASS.

- [ ] **Step 17: Commit CLI sync wiring**

```bash
rtk git add contrib/lidarr/src/musefs_lidarr/cli_sync.py contrib/lidarr/tests/test_cli.py
rtk git commit -m "feat(lidarr): run API-backed sync from CLI"
```

---

### Task 9: Path Gate Against Real musefs

**Files:**
- Create: `contrib/lidarr/tests/test_path_gate.py`

- [ ] **Step 1: Write opt-in path gate**

Create `contrib/lidarr/tests/test_path_gate.py`:

```python
import os
import shutil
import subprocess

import pytest
from musefs_common import connect, realpath_key

pytestmark = pytest.mark.musefs_bin


def test_symlink_scan_matches_real_backing_path(tmp_path):
    musefs_bin = shutil.which("musefs")
    if musefs_bin is None:
        pytest.skip("musefs binary not on PATH")

    source = tmp_path / "download.flac"
    destination = tmp_path / "library" / "Artist" / "download.flac"
    db_path = tmp_path / "musefs.db"
    destination.parent.mkdir(parents=True)

    subprocess.run(
        [
            "ffmpeg",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.2",
            "-y",
            str(source),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    destination.symlink_to(source)

    subprocess.run([musefs_bin, "scan", str(destination), "--db", str(db_path)], check=True)

    conn = connect(str(db_path))
    try:
        rows = conn.execute("SELECT backing_path FROM tracks").fetchall()
    finally:
        conn.close()

    assert rows == [(realpath_key(source),)]
```

- [ ] **Step 2: Run deselected default tests**

Run: `cd contrib/lidarr && rtk python -m pytest tests/test_path_gate.py -v`

Expected: `1 deselected` because `musefs_bin` tests are opt-in.

- [ ] **Step 3: Run the opt-in gate when tools are available**

Run: `rtk cargo build` from repo root, then `cd contrib/lidarr && rtk python -m pytest -m musefs_bin tests/test_path_gate.py -v`.

Expected: PASS when `musefs` is on `PATH` and `ffmpeg` exists; SKIP if the binary is not on `PATH`.

- [ ] **Step 4: Commit**

```bash
rtk git add contrib/lidarr/tests/test_path_gate.py
rtk git commit -m "test(lidarr): add symlink path gate"
```

---

### Task 10: Documentation

**Files:**
- Create: `contrib/lidarr/README.md`
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`
- Modify: `CONTRIBUTING.md`
- Modify: `contrib/python-musefs/README.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Create Lidarr README**

Create `contrib/lidarr/README.md` with:

````markdown
# lidarr-musefs

A Lidarr integration that lets Lidarr import into a placeholder library tree
while musefs serves the real consumer-facing, re-tagged FUSE view.

The supported workflow keeps Lidarr as the downloader, matcher, and metadata
source, but prevents Lidarr from copying, moving, or rewriting backing audio
bytes. Lidarr's destination tree exists so Lidarr can track files. Point
Navidrome, Plex, Jellyfin, or other consumers at the musefs mount instead.

## Required Lidarr settings

- Settings -> Media Management -> Import Using Script: enabled.
- Import Script Path: `musefs-lidarr-import`.
- Metadata Provider -> Write Audio Tags: `Never`.
- File Date: `None`.
- Linux permission management: disabled.

Do not rely on Lidarr's built-in "Use Hardlinks instead of Copy" for this
workflow. Lidarr uses a hardlink-or-copy transfer mode internally, so a hardlink
failure can copy bytes. `musefs-lidarr-import` creates the destination entry
itself and fails closed.

## Environment

Import script:

```bash
MUSEFS_LIDARR_LINK_MODE=symlink   # default; use hardlink only if symlinks are unsuitable
```

Sync script:

```bash
MUSEFS_DB=/path/to/musefs.db
MUSEFS_BIN=musefs
MUSEFS_LIDARR_URL=http://localhost:8686
MUSEFS_LIDARR_API_KEY=your-api-key
MUSEFS_LIDARR_AUTOSCAN=1
```

API keys are redacted from logs and errors.

## Lidarr Custom Script

Configure a Custom Script notification:

- On Release Import: enabled.
- On Rename: enabled.
- Path: `musefs-lidarr-sync`.

Test events exit successfully without touching files or the database.

## Doctor

Run:

```bash
musefs-lidarr-sync --doctor
```

The doctor checks Lidarr's API for:

- `writeAudioTags = no`
- `fileDate = none`
- `setPermissionsLinux = false`

If `MUSEFS_LIDARR_URL` and `MUSEFS_LIDARR_API_KEY` are not configured, manually
verify the settings above before syncing.

## Smoke test

1. Build and install musefs.
2. Install `python-musefs` and `lidarr-musefs` into the environment Lidarr uses
   for custom scripts.
3. Configure Import Using Script and Custom Script as described above.
4. Import a small album.
5. Confirm Lidarr's destination entry is a symlink by default.
6. Run `musefs mount /tmp/mnt --db "$MUSEFS_DB"`.
7. Confirm the mount shows Lidarr metadata.
8. Confirm the source file's bytes and mtime did not change.
````

- [ ] **Step 2: Update root README**

Modify the integration mention near the beets/Picard references to include:

```markdown
or via the [beets plugin](contrib/beets/README.md),
[Picard plugin](contrib/picard/README.md), or
[Lidarr integration](contrib/lidarr/README.md)
```

Add one short paragraph in the integrations section:

```markdown
The Lidarr integration uses Lidarr's Import Using Script hook to create a
symlink placeholder tree, then syncs Lidarr metadata into the musefs store from
Custom Script events. Media servers should point at the musefs mount, not the
Lidarr placeholder tree.
```

- [ ] **Step 3: Update architecture docs**

Modify `ARCHITECTURE.md` contrib ecosystem paragraph to mention Lidarr:

```markdown
The [Lidarr integration](contrib/lidarr/README.md) uses the same shared library
from a Custom Script workflow. Its Lidarr destination tree is a tracking aid
made of symlinks by default; musefs remains the consumer-facing filesystem.
```

- [ ] **Step 4: Update contributing docs**

In `CONTRIBUTING.md` Python plugin commands, add:

```bash
# lidarr: python-musefs is UNPUBLISHED — install the local lib first
cd contrib/lidarr && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests
```

Add gotcha:

```markdown
- The Lidarr real-instance smoke test is a release gate, not a default CI job.
  It verifies Lidarr accepts script-created symlink destinations and emits the
  expected Custom Script event.
```

- [ ] **Step 5: Update python-musefs README**

Add Lidarr to the consumers list:

```markdown
- **Lidarr** depends on this package via pip (`contrib/lidarr/pyproject.toml`).
```

- [ ] **Step 6: Update changelog**

Add a top entry:

```markdown
- Added a Lidarr integration design and implementation path for symlink-based
  imports plus musefs metadata sync.
```

- [ ] **Step 7: Run docs grep checks**

Run: `rtk rg -n "Lidarr|lidarr" README.md ARCHITECTURE.md CONTRIBUTING.md contrib/python-musefs/README.md contrib/lidarr/README.md CHANGELOG.md`

Expected: output includes all six documentation locations.

- [ ] **Step 8: Commit**

```bash
rtk git add contrib/lidarr/README.md README.md ARCHITECTURE.md CONTRIBUTING.md contrib/python-musefs/README.md CHANGELOG.md
rtk git commit -m "docs: add Lidarr integration guide"
```

---

### Task 11: CI Wiring and Full Verification

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add Lidarr CI job**

In `.github/workflows/ci.yml`, insert this job after `beets`:

```yaml
  lidarr:
    needs: changes
    if: needs.changes.outputs.src == 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Install Ruff
        run: pip install ruff
      - name: Lint
        run: |
          ruff check contrib/lidarr/
          ruff format --check contrib/lidarr/
      - name: Install python-musefs (local, unpublished dependency)
        run: pip install -e contrib/python-musefs
      - name: Install Lidarr integration
        run: pip install -e "contrib/lidarr[test]"
      - name: Test
        run: python -m pytest contrib/lidarr/tests -v
```

Update `ci-ok`:

```yaml
    needs: [changes, check, interop, python-musefs, beets, lidarr, picard, e2e]
```

- [ ] **Step 2: Run YAML grep verification**

Run: `rtk rg -n "lidarr|contrib/lidarr" .github/workflows/ci.yml`

Expected: output shows the `lidarr` job and the `ci-ok` dependency.

- [ ] **Step 3: Run package tests**

Run:

```bash
cd contrib/lidarr
rtk python -m pytest -v
rtk ruff check .
rtk ruff format --check .
```

Expected: PASS.

- [ ] **Step 4: Run shared Python tests**

Run:

```bash
cd contrib/python-musefs
rtk python -m pytest -v
```

Expected: PASS.

- [ ] **Step 5: Run full workspace smoke**

Run from repo root:

```bash
rtk cargo test --workspace
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
rtk git add .github/workflows/ci.yml
rtk git commit -m "ci: test Lidarr integration"
```

---

### Task 12: Release-Gate Manual Smoke Notes

**Files:**
- Modify: `contrib/lidarr/README.md`
- Modify: `docs/superpowers/plans/2026-06-07-lidarr-integration.md` only if the implementation discovers a required command correction while executing this task.

- [ ] **Step 1: Verify README has release gate checklist**

Run: `rtk rg -n "Smoke test|mtime|symlink|Custom Script" contrib/lidarr/README.md`

Expected: output includes the smoke test section and checks for symlink, musefs mount metadata, and source mtime.

- [ ] **Step 2: If a real Lidarr instance is available, run the smoke test**

Run the manual flow from `contrib/lidarr/README.md`. Record these observations in the PR description or release notes:

```text
Lidarr version:
Import link mode:
Destination entry type:
Source byte checksum before:
Source byte checksum after:
Source mtime before:
Source mtime after:
musefs mount metadata verified:
```

Expected:

```text
Destination entry type: symlink
Source byte checksum before: same as after
Source mtime before: same as after
musefs mount metadata verified: yes
```

- [ ] **Step 3: Do not block local development if Lidarr is unavailable**

If a real Lidarr instance is not available, do not mark the feature release-ready. Leave the release-gate observation for the PR/release checklist. The implementation can still merge behind normal automated tests if the project owner accepts the deferred release gate.

- [ ] **Step 4: Final verification**

Run:

```bash
rtk git status --short
cd contrib/lidarr && rtk python -m pytest -v && rtk ruff check . && rtk ruff format --check .
```

Expected: tests/lint pass; `git status --short` is clean or contains only intentional uncommitted release-gate notes requested by the maintainer.

---

## Spec Coverage Checklist

- Package and contrib layering: Tasks 1, 10, 11.
- Import script symlink default and hardlink opt-in: Task 2.
- Lidarr event parsing for Test, AlbumDownload, Rename, unsupported events: Task 3.
- API client, API key redaction, preflight: Task 4.
- Path-to-track matching and metadata mapping: Task 5.
- DB sync, scan wrapper, symlink/hardlink rename semantics: Tasks 6 and 8.
- CLI entry points: Task 7.
- Path gate against real `musefs`: Task 9.
- Documentation updates: Task 10.
- CI updates: Task 11.
- Real-Lidarr release gate: Task 12.

## Notes for Implementers

- Before coding, create or verify an isolated worktree using `superpowers:using-git-worktrees` if the current workspace has unrelated work.
- Execute tasks in order. Each task is intended to commit independently.
- Keep `python-musefs` as the only DB writer; do not write `tags`, `art`, or `track_art` directly from Lidarr-specific modules except through `musefs_common`.
- Never add a fallback from hardlink mode to copy mode.
- Do not print `MUSEFS_LIDARR_API_KEY`; use `<redacted>` in all user-visible output.
