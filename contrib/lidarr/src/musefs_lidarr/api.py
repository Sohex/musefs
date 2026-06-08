from __future__ import annotations

import json
import os
from dataclasses import dataclass
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from .errors import ConfigError, LidarrApiError


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

    def __init__(self, config: LidarrConfig, *, opener=urlopen, timeout: int = 15):
        if not config.url or not config.api_key:
            raise ConfigError("Lidarr API configuration is required")
        self._base = config.url.rstrip("/")
        self._api_key = config.api_key
        self._opener = opener
        self._timeout = timeout

    def get_json(self, path: str, params: dict[str, object] | None = None):
        """GET ``path`` with optional query params; return parsed JSON.

        Raises ``LidarrApiError`` on HTTP, network, or JSON-decode failure.
        """
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
