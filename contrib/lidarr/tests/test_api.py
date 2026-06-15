import json
from urllib.error import HTTPError, URLError

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

    client = LidarrClient(
        LidarrConfig(url="http://lidarr.local/", api_key="secret"), opener=opener
    )

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


class RawResponse:
    def __init__(self, payload: bytes):
        self.payload = payload

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def read(self):
        return self.payload


def _flaky_opener(failures):
    """Opener that raises each exception in ``failures`` (oldest first) before
    finally returning ``FakeResponse({"ok": True})``."""
    pending = list(failures)
    calls = []

    def opener(request, timeout):
        calls.append(request.full_url)
        if pending:
            raise pending.pop(0)
        return FakeResponse({"ok": True})

    return opener, calls


def test_get_json_retries_transient_url_error_then_succeeds():
    opener, calls = _flaky_opener([URLError("connection refused"), URLError("still down")])
    sleeps = []
    client = LidarrClient(
        LidarrConfig(url="http://lidarr.local", api_key="secret"),
        opener=opener,
        retries=3,
        sleep=sleeps.append,
    )

    assert client.get_json("/api/v1/album/1") == {"ok": True}
    assert len(calls) == 3
    assert len(sleeps) == 2


def test_get_json_retries_on_5xx_then_succeeds():
    err = HTTPError("http://lidarr.local/api/v1/album/1", 503, "Service Unavailable", None, None)
    opener, calls = _flaky_opener([err])
    client = LidarrClient(
        LidarrConfig(url="http://lidarr.local", api_key="secret"),
        opener=opener,
        retries=3,
        sleep=lambda _: None,
    )

    assert client.get_json("/api/v1/album/1") == {"ok": True}
    assert len(calls) == 2


def test_get_json_does_not_retry_on_4xx():
    err = HTTPError("http://lidarr.local/api/v1/album/1", 401, "Unauthorized", None, None)
    opener, calls = _flaky_opener([err])
    client = LidarrClient(
        LidarrConfig(url="http://lidarr.local", api_key="secret"),
        opener=opener,
        retries=3,
        sleep=lambda _: None,
    )

    with pytest.raises(LidarrApiError):
        client.get_json("/api/v1/album/1")
    assert len(calls) == 1


def test_get_json_gives_up_after_exhausting_retries():
    opener, calls = _flaky_opener([URLError("down")] * 5)
    client = LidarrClient(
        LidarrConfig(url="http://lidarr.local", api_key="secret"),
        opener=opener,
        retries=3,
        sleep=lambda _: None,
    )

    with pytest.raises(LidarrApiError):
        client.get_json("/api/v1/album/1")
    assert len(calls) == 3


def test_media_cover_fetches_raw_bytes_with_api_key():
    captured = {}

    def opener(request, timeout):
        captured["url"] = request.full_url
        captured["headers"] = dict(request.header_items())
        return RawResponse(b"\xff\xd8\xff\xe0jpegdata")

    client = LidarrClient(
        LidarrConfig(url="http://lidarr.local/", api_key="secret"), opener=opener
    )

    data = client.media_cover("/MediaCover/Albums/20/cover.jpg?lastModified=1")

    assert data == b"\xff\xd8\xff\xe0jpegdata"
    assert captured["url"] == "http://lidarr.local/MediaCover/Albums/20/cover.jpg?lastModified=1"
    assert captured["headers"]["X-api-key"] == "secret"
