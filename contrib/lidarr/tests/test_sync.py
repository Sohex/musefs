from musefs_common import SCAN_TIMEOUT_SECONDS, ArtImage, connect, realpath_key

from musefs_lidarr.errors import LidarrApiError
from musefs_lidarr.events import EventType, LidarrEvent
from musefs_lidarr.import_link import LinkMode
from musefs_lidarr.sync import (
    SyncConfig,
    _collect_album_art,
    collect_all_payloads,
    collect_event_payloads,
    config_from_env,
    scan_if_enabled,
    sync_event_with_payloads,
    sync_records,
    sync_rename_prune,
)


class FakeClient:
    def __init__(self, *, track_files, tracks, albums, artists, art=None):
        self.track_file_calls = []
        self.track_calls = []
        self.artist_calls = []
        self.album_calls = []
        self.media_cover_calls = []
        self._track_files = track_files
        self._tracks = tracks
        self._albums = {album["id"]: album for album in albums}
        self._artists = {artist["id"]: artist for artist in artists}
        self._art = art or {}

    def media_cover(self, url):
        self.media_cover_calls.append(url)
        return self._art[url]

    def track_files(self, **kwargs):
        self.track_file_calls.append(kwargs)
        return self._track_files

    def tracks(self, **kwargs):
        self.track_calls.append(kwargs)
        return self._tracks

    def album(self, album_id):
        self.album_calls.append(album_id)
        return self._albums[album_id]

    def artists(self):
        return list(self._artists.values())

    def artist(self, artist_id):
        self.artist_calls.append(artist_id)
        return self._artists[artist_id]


def test_config_from_env_reads_required_values(tmp_path):
    config = config_from_env(
        {
            "MUSEFS_DB": str(tmp_path / "musefs.db"),
            "MUSEFS_BIN": "musefs-dev",
            "MUSEFS_LIDARR_AUTOSCAN": "0",
            "MUSEFS_LIDARR_LINK_MODE": "hardlink",
        }
    )

    assert config == SyncConfig(
        db_path=str(tmp_path / "musefs.db"),
        link_mode=LinkMode.HARDLINK,
        autoscan=False,
        musefs_bin="musefs-dev",
    )


def test_scan_if_enabled_skips_when_disabled(tmp_path):
    calls = []
    config = SyncConfig(
        db_path=str(tmp_path / "m.db"),
        link_mode=LinkMode.SYMLINK,
        autoscan=False,
    )

    scan_if_enabled(
        config=config,
        paths=["/music/a.flac"],
        runner=lambda *args, **kwargs: calls.append(args),
    )

    assert calls == []


def test_scan_if_enabled_calls_runner(tmp_path):
    calls = []
    config = SyncConfig(
        db_path=str(tmp_path / "m.db"),
        link_mode=LinkMode.SYMLINK,
        autoscan=True,
        musefs_bin="musefs-dev",
    )

    scan_if_enabled(
        config=config,
        paths=["/music/a.flac"],
        runner=lambda binary, db_path, targets, *, timeout=None: calls.append(
            (binary, db_path, targets)
        ),
    )

    assert calls == [("musefs-dev", str(tmp_path / "m.db"), ["/music/a.flac"])]


def test_scan_if_enabled_passes_shared_timeout(tmp_path):
    captured = {}
    config = SyncConfig(
        db_path=str(tmp_path / "m.db"),
        link_mode=LinkMode.SYMLINK,
        autoscan=True,
    )

    def fake_runner(binary, db_path, targets, *, timeout=None):
        captured["timeout"] = timeout

    scan_if_enabled(config=config, paths=["/music/a.flac"], runner=fake_runner)

    assert captured["timeout"] == SCAN_TIMEOUT_SECONDS == 120


def test_sync_records_writes_tags(
    db_path, make_track, sample_track_file, sample_track, sample_album, sample_artist
):
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
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert stats.synced == 1
    conn = connect(db_path)
    try:
        rows = conn.execute("SELECT key, value FROM tags ORDER BY key, ordinal").fetchall()
    finally:
        conn.close()
    assert ("title", "Wildlife Analysis") in rows
    assert ("genre", "Electronic") in rows


def test_sync_records_counts_and_logs_missing_row_as_skipped(
    db_path, sample_track_file, sample_track, sample_album, sample_artist, capsys
):
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
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert stats.synced == 0
    assert stats.skipped == 1
    captured = capsys.readouterr()
    assert realpath_key(sample_track_file["path"]) in captured.err
    assert "no matching musefs track row after scan" in captured.err


def test_sync_records_logs_mapping_skipped_path(db_path, sample_track_file, capsys):
    missing_path = sample_track_file["path"].replace("01 - Wildlife Analysis", "02 - Missing")
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[missing_path],
        artist_id=10,
        album_id=20,
    )
    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK, autoscan=False)

    stats = sync_records(
        config=config,
        event=event,
        track_files=[],
        tracks=[],
        albums_by_id={},
        artists_by_id={},
    )

    assert stats.synced == 0
    assert stats.skipped == 1
    captured = capsys.readouterr()
    assert missing_path in captured.err
    assert "no matching Lidarr track file" in captured.err


def test_sync_records_logs_invalid_record(
    db_path, make_track, sample_track_file, sample_track, sample_album, sample_artist, capsys
):
    key = realpath_key(sample_track_file["path"])
    make_track(key)
    bad_track = {**sample_track, "title": "x" * 262145}  # over the value-length CHECK
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
        tracks=[bad_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
    )

    assert stats.synced == 0
    assert stats.skipped_invalid == 1
    captured = capsys.readouterr()
    assert key in captured.err
    assert "invalid" in captured.err.lower()


def test_symlink_rename_does_not_prune_previous_placeholder(
    db_path, make_track, sample_track_file, tmp_path
):
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


def test_sync_event_with_payloads_scans_then_syncs(
    db_path, make_track, sample_track_file, sample_track, sample_album, sample_artist, monkeypatch
):
    key = realpath_key(sample_track_file["path"])
    make_track(key)
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )
    config = SyncConfig(
        db_path=db_path,
        link_mode=LinkMode.SYMLINK,
        autoscan=True,
        musefs_bin="musefs-dev",
    )
    calls = []

    def fake_scan(binary, db_path_arg, targets, *, timeout=None):
        calls.append(("scan", binary, db_path_arg, targets))

    def fake_prune(*, config, previous_paths):
        calls.append(("prune", config, previous_paths))
        return 0

    monkeypatch.setattr("musefs_lidarr.sync.sync_rename_prune", fake_prune)

    stats = sync_event_with_payloads(
        config=config,
        event=event,
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
        scanner=fake_scan,
    )

    assert stats.synced == 1
    assert calls[0] == ("scan", "musefs-dev", db_path, [sample_track_file["path"]])
    assert calls[1][0] == "prune"


def test_collect_event_payloads_queries_by_album_when_available(
    sample_track_file, sample_track, sample_album, sample_artist
):
    client = FakeClient(
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums=[sample_album],
        artists=[sample_artist],
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
    assert payloads.albums_by_id == {20: sample_album}
    assert payloads.artists_by_id == {10: sample_artist}


def test_collect_event_payloads_fetches_all_albums_for_artist_rename(
    sample_track_file, sample_track, sample_album, sample_artist, tmp_path
):
    second_album = dict(sample_album, id=21, title="Geogaddi")
    second_path = tmp_path / "library" / "02 - Second.flac"
    second_path.write_bytes(b"audio")
    second_track_file = dict(
        sample_track_file,
        id=31,
        albumId=21,
        path=str(second_path),
    )
    second_track = dict(
        sample_track,
        id=41,
        albumId=21,
        trackFileId=31,
        title="Ready Lets Go",
    )
    client = FakeClient(
        track_files=[sample_track_file, second_track_file],
        tracks=[sample_track, second_track],
        albums=[sample_album, second_album],
        artists=[sample_artist],
    )
    event = LidarrEvent(
        event_type=EventType.RENAME,
        raw_type="Rename",
        paths=[sample_track_file["path"], second_track_file["path"]],
        artist_id=10,
    )

    payloads = collect_event_payloads(client=client, event=event)

    assert client.track_file_calls == [{"artist_id": 10}]
    assert client.track_calls == [{"artist_id": 10}]
    assert payloads.albums_by_id == {20: sample_album, 21: second_album}
    assert payloads.artists_by_id == {10: sample_artist}


def test_collect_all_payloads_queries_each_artist(
    sample_track_file, sample_track, sample_album, sample_artist
):
    client = FakeClient(
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums=[sample_album],
        artists=[sample_artist],
    )

    payloads = collect_all_payloads(client=client)

    assert client.track_file_calls == [{"artist_id": 10}]
    assert client.track_calls == [{"artist_id": 10}]
    assert payloads.track_files == [sample_track_file]
    assert payloads.tracks == [sample_track]
    assert payloads.paths == [sample_track_file["path"]]


def test_sync_records_writes_album_art(
    db_path, make_track, sample_track_file, sample_track, sample_album, sample_artist
):
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
    art = ArtImage(data=b"\xff\xd8\xff\xe0cover", mime="image/jpeg")

    stats = sync_records(
        config=config,
        event=event,
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums_by_id={20: sample_album},
        artists_by_id={10: sample_artist},
        art_by_album_id={20: art},
    )

    assert stats.art_linked == 1
    conn = connect(db_path)
    try:
        rows = conn.execute(
            "SELECT a.data, a.mime, ta.picture_type FROM track_art ta "
            "JOIN art a ON a.id = ta.art_id"
        ).fetchall()
    finally:
        conn.close()
    assert rows == [(b"\xff\xd8\xff\xe0cover", "image/jpeg", 3)]


def test_collect_event_payloads_fetches_album_art(
    sample_track_file, sample_track, sample_album, sample_artist
):
    album = dict(
        sample_album,
        images=[{"coverType": "cover", "url": "/MediaCover/Albums/20/cover.jpg"}],
    )
    client = FakeClient(
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums=[album],
        artists=[sample_artist],
        art={"/MediaCover/Albums/20/cover.jpg": b"\xff\xd8\xff\xe0cover"},
    )
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )

    payloads = collect_event_payloads(client=client, event=event)

    assert client.media_cover_calls == ["/MediaCover/Albums/20/cover.jpg"]
    art = payloads.art_by_album_id[20]
    assert art.data == b"\xff\xd8\xff\xe0cover"
    assert art.mime == "image/jpeg"


def test_collect_event_payloads_skips_album_without_art(
    sample_track_file, sample_track, sample_album, sample_artist
):
    client = FakeClient(
        track_files=[sample_track_file],
        tracks=[sample_track],
        albums=[sample_album],
        artists=[sample_artist],
    )
    event = LidarrEvent(
        event_type=EventType.ALBUM_DOWNLOAD,
        raw_type="AlbumDownload",
        paths=[sample_track_file["path"]],
        artist_id=10,
        album_id=20,
    )

    payloads = collect_event_payloads(client=client, event=event)

    assert client.media_cover_calls == []
    assert payloads.art_by_album_id == {}


def test_collect_album_art_logs_and_skips_fetch_failure(sample_album, sample_artist, capsys):
    album = dict(
        sample_album,
        images=[{"coverType": "cover", "url": "/MediaCover/Albums/20/cover.jpg"}],
    )

    class FailingClient:
        def media_cover(self, url):
            raise LidarrApiError("boom")

    result = _collect_album_art(FailingClient(), {20: album})

    assert result == {}
    assert "album 20" in capsys.readouterr().err


def test_prune_deleted_album_removes_matching_rows(db_path, tmp_path):
    from musefs_common import connect
    from musefs_common.store import replace_tags
    from musefs_lidarr.events import EventType, LidarrEvent
    from musefs_lidarr.import_link import LinkMode
    from musefs_lidarr.sync import SyncConfig, prune_deleted

    backing = tmp_path / "a.flac"
    backing.write_bytes(b"audio")  # backing file stays on disk

    conn = connect(db_path)
    try:
        a = conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
            "backing_size, backing_mtime_ns, updated_at) VALUES (?, 'flac', 0, 0, 0, 0, 0)",
            (str(backing),),
        ).lastrowid
        b = conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
            "backing_size, backing_mtime_ns, updated_at) VALUES ('/m/b.flac', 'flac', 0, 0, 0, 0, 0)",
        ).lastrowid
        replace_tags(conn, a, [("musicbrainz_albumid", "rg-1")])
        replace_tags(conn, b, [("musicbrainz_albumid", "rg-2")])
        conn.commit()
    finally:
        conn.close()

    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK)
    event = LidarrEvent(
        event_type=EventType.ALBUM_DELETED, raw_type="AlbumDeleted", album_mbid="rg-1"
    )
    pruned = prune_deleted(config=config, event=event)

    assert pruned == 1
    assert backing.exists()  # invariant: backing bytes untouched
    conn = connect(db_path)
    try:
        ids = {row[0] for row in conn.execute("SELECT id FROM tracks")}
        assert ids == {b}
    finally:
        conn.close()


def test_prune_deleted_artist_removes_all_artist_rows(db_path):
    from musefs_common import connect
    from musefs_common.store import replace_tags
    from musefs_lidarr.events import EventType, LidarrEvent
    from musefs_lidarr.import_link import LinkMode
    from musefs_lidarr.sync import SyncConfig, prune_deleted

    conn = connect(db_path)
    try:
        ids = []
        for i, art in enumerate(["art-1", "art-1", "art-2"]):
            tid = conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, "
                "backing_size, backing_mtime_ns, updated_at) VALUES (?, 'flac', 0, 0, 0, 0, 0)",
                (f"/m/{i}.flac",),
            ).lastrowid
            replace_tags(conn, tid, [("musicbrainz_artistid", art)])
            ids.append(tid)
        conn.commit()
    finally:
        conn.close()

    config = SyncConfig(db_path=db_path, link_mode=LinkMode.SYMLINK)
    event = LidarrEvent(
        event_type=EventType.ARTIST_DELETED, raw_type="ArtistDeleted", artist_mbid="art-1"
    )
    assert prune_deleted(config=config, event=event) == 2
