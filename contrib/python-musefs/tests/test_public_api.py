import musefs_common


def test_version_is_package_semver_not_schema_version():
    assert musefs_common.__version__ == "1.2.0"
    assert musefs_common.__version__ != str(musefs_common.EXPECTED_USER_VERSION)


def test_public_api_surface():
    expected = {
        "EXPECTED_USER_VERSION",
        "MAX_ART_BYTES",
        "SCAN_TIMEOUT_SECONDS",
        "SchemaMismatch",
        "ScanError",
        "realpath_key",
        "run_scan",
        "connect",
        "check_schema_version",
        "track_id_for_path",
        "track_ids_for_paths",
        "track_ids_by_tag",
        "tags_for_track",
        "TagRow",
        "prune_missing",
        "delete_tracks",
        "replace_tags",
        "upsert_art",
        "replace_track_art",
        "sniff_mime",
        "ArtImage",
        "Record",
        "SyncStats",
        "sync_one",
        "sync_files",
    }
    assert expected <= set(musefs_common.__all__)
    for name in expected:
        assert hasattr(musefs_common, name), name
