from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from musefs_common import MAX_ART_BYTES

from .errors import ConfigError, LidarrApiError

# Lidarr custom scripts are fire-and-forget: a transient API error or a restart
# mid-import otherwise silently loses the sync, so retry these with backoff.
_RETRYABLE_STATUS = frozenset({408, 429, 500, 502, 503, 504})
_RETRY_BACKOFF_BASE = 0.5


def redacted(value: str | None) -> str:
    """Return ``"<redacted>"`` for a non-empty value, else ``"<missing>"``."""
    return "<redacted>" if value else "<missing>"


@dataclass(frozen=True)
class LidarrConfig:
    """Lidarr API connection settings (URL and API key)."""

    url: str | None = None
    api_key: str | None = None

    @classmethod
    def from_env(cls, environ: dict[str, str] | None = None) -> "LidarrConfig":
        """Read URL/key from ``MUSEFS_LIDARR_URL``/``MUSEFS_LIDARR_API_KEY``.

        Raises ``ConfigError`` if only one of the two is set.
        """
        env = os.environ if environ is None else environ
        url = env.get("MUSEFS_LIDARR_URL") or None
        api_key = env.get("MUSEFS_LIDARR_API_KEY") or None
        if bool(url) != bool(api_key):
            raise ConfigError("MUSEFS_LIDARR_URL and MUSEFS_LIDARR_API_KEY must be set together")
        return cls(url=url, api_key=api_key)

    @property
    def enabled(self) -> bool:
        """True when both URL and API key are present."""
        return bool(self.url and self.api_key)


@dataclass(frozen=True)
class PreflightResult:
    """Outcome of the Lidarr settings preflight: ``ok`` plus any error strings."""

    ok: bool
    errors: list[str]


class LidarrClient:
    """Minimal read-only client for the Lidarr v1 REST API."""

    def __init__(
        self,
        config: LidarrConfig,
        *,
        opener=urlopen,
        timeout: int = 15,
        retries: int = 3,
        sleep=time.sleep,
    ):
        if not config.url or not config.api_key:
            raise ConfigError("Lidarr API configuration is required")
        self._base = config.url.rstrip("/")
        self._api_key = config.api_key
        self._opener = opener
        self._timeout = timeout
        self._retries = max(1, retries)
        self._sleep = sleep

    def get_json(self, path: str, params: dict[str, object] | None = None):
        """GET ``path`` with optional query params; return parsed JSON.

        Raises ``LidarrApiError`` on HTTP, network, or JSON-decode failure.
        """
        try:
            return json.loads(self._request(self._url(path, params)).decode("utf-8"))
        except json.JSONDecodeError as exc:
            raise LidarrApiError("Lidarr API returned invalid JSON") from exc

    def media_cover(self, url: str) -> bytes:
        """Fetch raw cover-art bytes for a Lidarr image ``url``.

        ``url`` is the server-relative path from an album's ``images`` entry
        (e.g. ``/MediaCover/Albums/20/cover.jpg``); an absolute ``remoteUrl`` is
        used as-is. The API key is sent only for server-local paths — absolute
        URLs point at third-party hosts (coverartarchive.org, etc.) and must not
        carry the Lidarr key. The body is capped at ``MAX_ART_BYTES`` so a
        third-party host cannot make us buffer an unbounded image. Retries
        transient failures like :meth:`get_json`.
        """
        if url.startswith(("http://", "https://")):
            return self._request(url, send_key=False, max_bytes=MAX_ART_BYTES)
        return self._request(f"{self._base}{url}", max_bytes=MAX_ART_BYTES)

    def _url(self, path: str, params: dict[str, object] | None = None) -> str:
        query = ""
        if params:
            clean = {k: v for k, v in params.items() if v is not None}
            if clean:
                query = "?" + urlencode(clean, doseq=True)
        return f"{self._base}{path}{query}"

    def _request(self, url: str, *, send_key: bool = True, max_bytes: int | None = None) -> bytes:
        """GET ``url``, returning the raw response body.

        The ``X-Api-Key`` header is sent only when ``send_key`` is true, so an
        absolute third-party URL never carries the Lidarr key. When ``max_bytes``
        is set, a response whose declared or actual length exceeds it fails with
        ``LidarrApiError`` rather than being buffered in full.

        Retries up to ``self._retries`` attempts with exponential backoff on
        transient failures (network errors, timeouts, and the 5xx/429/408 HTTP
        statuses); non-transient HTTP errors fail fast. Every failing path raises
        ``LidarrApiError``.
        """
        headers = {"X-Api-Key": self._api_key} if send_key else {}
        request = Request(url, headers=headers)
        last_exc: Exception | None = None
        message = "Lidarr API request failed"
        for attempt in range(self._retries):
            try:
                with self._opener(request, timeout=self._timeout) as response:
                    return self._read_capped(response, max_bytes)
            except HTTPError as exc:
                message = (
                    f"Lidarr API request failed with HTTP {exc.code}; "
                    f"api_key={redacted(self._api_key)}"
                )
                if exc.code not in _RETRYABLE_STATUS:
                    raise LidarrApiError(message) from exc
                last_exc = exc
            except (URLError, TimeoutError) as exc:
                last_exc = exc
                message = f"Lidarr API request failed: {getattr(exc, 'reason', exc)}"
            if attempt + 1 < self._retries:
                self._sleep(_RETRY_BACKOFF_BASE * (2**attempt))
        raise LidarrApiError(message) from last_exc

    def _read_capped(self, response, max_bytes: int | None) -> bytes:
        """Read ``response`` body, rejecting one larger than ``max_bytes``.

        A declared ``Content-Length`` over the cap fails before any body is read;
        otherwise at most ``max_bytes + 1`` bytes are buffered to detect overrun.
        """
        if max_bytes is None:
            return response.read()
        declared = response.headers.get("Content-Length")
        if declared is not None:
            try:
                declared_len = int(declared)
            except ValueError:
                declared_len = None
            if declared_len is not None and declared_len > max_bytes:
                raise LidarrApiError(
                    f"Lidarr cover art exceeds {max_bytes} bytes (Content-Length {declared_len})"
                )
        body = response.read(max_bytes + 1)
        if len(body) > max_bytes:
            raise LidarrApiError(f"Lidarr cover art exceeds {max_bytes} bytes")
        return body

    def media_management_config(self):
        """Return Lidarr's media-management config."""
        return self.get_json("/api/v1/config/mediamanagement")

    def metadata_provider_config(self):
        """Return Lidarr's metadata-provider config."""
        return self.get_json("/api/v1/config/metadataprovider")

    def track_files(self, *, artist_id=None, album_id=None, track_file_ids=None):
        """Return track files filtered by artist, album, or track-file ids."""
        params = {}
        if artist_id is not None:
            params["artistId"] = artist_id
        if album_id is not None:
            params["albumId"] = album_id
        if track_file_ids:
            params["trackFileIds"] = list(track_file_ids)
        return self.get_json("/api/v1/trackfile", params)

    def tracks(self, *, artist_id=None, album_id=None, track_ids=None):
        """Return tracks filtered by artist, album, or track ids."""
        params = {}
        if artist_id is not None:
            params["artistId"] = artist_id
        if album_id is not None:
            params["albumId"] = album_id
        if track_ids:
            params["trackIds"] = list(track_ids)
        return self.get_json("/api/v1/track", params)

    def album(self, album_id: int):
        """Return a single album by id."""
        return self.get_json(f"/api/v1/album/{album_id}")

    def artists(self):
        """Return all artists in the Lidarr library."""
        return self.get_json("/api/v1/artist")

    def artist(self, artist_id: int):
        """Return a single artist by id."""
        return self.get_json(f"/api/v1/artist/{artist_id}")


def _lower(value) -> str:
    return str(value).strip().lower()


def check_safe_settings(metadata: dict, media_management: dict) -> PreflightResult:
    """Verify Lidarr won't mutate backing files.

    Requires ``writeAudioTags=no``, ``fileDate=none``, and
    ``setPermissionsLinux`` falsy; collects a message per violation.
    """
    errors = []
    if _lower(metadata.get("writeAudioTags")) != "no":
        errors.append(f"writeAudioTags must be no, got {metadata.get('writeAudioTags')}")
    if _lower(media_management.get("fileDate")) != "none":
        errors.append(f"fileDate must be none, got {media_management.get('fileDate')}")
    if bool(media_management.get("setPermissionsLinux")):
        errors.append("setPermissionsLinux must be false")
    return PreflightResult(ok=not errors, errors=errors)


def run_preflight(client: LidarrClient) -> PreflightResult:
    """Fetch Lidarr's configs and run :func:`check_safe_settings`."""
    return check_safe_settings(
        metadata=client.metadata_provider_config(),
        media_management=client.media_management_config(),
    )
