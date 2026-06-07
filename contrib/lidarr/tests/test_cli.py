from __future__ import annotations

from urllib.error import HTTPError

from musefs_lidarr.api import LidarrClient
from musefs_lidarr.cli_import import run as run_import
from musefs_lidarr.cli_sync import run as run_sync


class FakeStats:
    def __init__(self, summary_text: str = "synced=1 skipped=0 art_linked=0 skipped_art=0"):
        self._summary_text = summary_text

    def summary(self) -> str:
        return self._summary_text


def test_import_cli_test_event_exits_zero(capsys):
    rc = run_import({"Lidarr_EventType": "Test"})

    assert rc == 0
    assert "test ok" in capsys.readouterr().out


def test_import_cli_creates_symlink(tmp_path):
    src = tmp_path / "src.flac"
    dst = tmp_path / "library" / "dst.flac"
    src.write_bytes(b"audio")

    rc = run_import(
        {
            "Lidarr_SourcePath": str(src),
            "Lidarr_DestinationPath": str(dst),
        }
    )

    assert rc == 0
    assert dst.is_symlink()


def test_sync_cli_test_event_exits_zero(capsys):
    rc = run_sync([], {"Lidarr_EventType": "Test"})

    assert rc == 0
    assert "test ok" in capsys.readouterr().out


def test_sync_cli_unsupported_event_exits_zero(capsys):
    rc = run_sync([], {"Lidarr_EventType": "Grab"})

    assert rc == 0
    assert "unsupported event" in capsys.readouterr().out


def test_sync_cli_track_retag_skips_without_db_or_api(capsys):
    rc = run_sync([], {"Lidarr_EventType": "TrackRetag", "Lidarr_Artist_Id": "5"})

    assert rc == 0
    captured = capsys.readouterr()
    assert "TrackRetag" in captured.err


def test_sync_cli_doctor_reports_success(capsys):
    class SafeClient:
        def metadata_provider_config(self):
            return {"writeAudioTags": "no"}

        def media_management_config(self):
            return {"fileDate": "none", "setPermissionsLinux": False}

    rc = run_sync(
        ["--doctor"],
        {
            "MUSEFS_LIDARR_URL": "http://lidarr.local",
            "MUSEFS_LIDARR_API_KEY": "secret",
        },
        client_factory=lambda config: SafeClient(),
    )

    assert rc == 0
    assert "doctor ok" in capsys.readouterr().out


def test_sync_cli_doctor_redacts_api_key(capsys):
    def opener(request, timeout):
        raise HTTPError(request.full_url, 401, "Unauthorized", hdrs=None, fp=None)

    rc = run_sync(
        ["--doctor"],
        {
            "MUSEFS_LIDARR_URL": "http://lidarr.local",
            "MUSEFS_LIDARR_API_KEY": "supersecret",
        },
        client_factory=lambda config: LidarrClient(config, opener=opener),
    )

    assert rc == 1
    captured = capsys.readouterr()
    assert "supersecret" not in captured.err
    assert "<redacted>" in captured.err


def test_sync_cli_skip_preflight_allows_sync(
    tmp_path, sample_track_file, sample_track, sample_album, sample_artist
):
    calls = []

    class UnsafeClient:
        def track_files(self, **kwargs):
            return [sample_track_file]

        def tracks(self, **kwargs):
            return [sample_track]

        def album(self, album_id):
            return sample_album

        def artist(self, artist_id):
            return sample_artist

        def metadata_provider_config(self):
            raise AssertionError("preflight should be skipped")

        def media_management_config(self):
            raise AssertionError("preflight should be skipped")

    def fake_sync(**kwargs):
        calls.append(kwargs)
        return FakeStats()

    rc = run_sync(
        ["--skip-lidarr-preflight"],
        {
            "Lidarr_EventType": "AlbumDownload",
            "Lidarr_Artist_Id": "10",
            "Lidarr_Album_Id": "20",
            "Lidarr_AddedTrackPaths": sample_track_file["path"],
            "MUSEFS_DB": str(tmp_path / "musefs.db"),
            "MUSEFS_LIDARR_URL": "http://lidarr.local",
            "MUSEFS_LIDARR_API_KEY": "secret",
        },
        client_factory=lambda config: UnsafeClient(),
        sync_runner=fake_sync,
    )

    assert rc == 0
    assert calls
    assert calls[0]["event"].raw_type == "AlbumDownload"


def test_sync_cli_api_backed_event_sync(
    tmp_path, sample_track_file, sample_track, sample_album, sample_artist, capsys
):
    calls = []

    class SafeClient:
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
        return FakeStats()

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
        client_factory=lambda config: SafeClient(),
        sync_runner=fake_sync,
    )

    assert rc == 0
    assert calls[0]["track_files"] == [sample_track_file]
    assert calls[0]["tracks"] == [sample_track]
    assert calls[0]["albums_by_id"] == {20: sample_album}
    assert calls[0]["artists_by_id"] == {10: sample_artist}
    assert (
        "musefs-lidarr-sync: synced=1 skipped=0 art_linked=0 skipped_art=0"
        in capsys.readouterr().out
    )


def test_sync_cli_all_runs_manual_backfill(
    tmp_path, sample_track_file, sample_track, sample_album, sample_artist, capsys
):
    calls = []

    class SafeClient:
        def artists(self):
            return [sample_artist]

        def track_files(self, **kwargs):
            return [sample_track_file]

        def tracks(self, **kwargs):
            return [sample_track]

        def album(self, album_id):
            return sample_album

        def metadata_provider_config(self):
            return {"writeAudioTags": "no"}

        def media_management_config(self):
            return {"fileDate": "none", "setPermissionsLinux": False}

    def fake_sync(**kwargs):
        calls.append(kwargs)
        return FakeStats()

    rc = run_sync(
        ["--all"],
        {
            "MUSEFS_DB": str(tmp_path / "musefs.db"),
            "MUSEFS_LIDARR_URL": "http://lidarr.local",
            "MUSEFS_LIDARR_API_KEY": "secret",
        },
        client_factory=lambda config: SafeClient(),
        sync_runner=fake_sync,
    )

    assert rc == 0
    assert calls[0]["event"].raw_type == "ManualAll"
    assert calls[0]["event"].paths == [sample_track_file["path"]]
    assert "doctor ok" in capsys.readouterr().out
