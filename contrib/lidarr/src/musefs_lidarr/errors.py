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
