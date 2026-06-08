from musefs_lidarr.env import lidarr_get


def test_lidarr_get_exact_match():
    assert lidarr_get({"Lidarr_EventType": "Test"}, "Lidarr_EventType") == "Test"


def test_lidarr_get_resolves_lowercase_keys():
    # Lidarr lowercases every env var name via its .NET StringDictionary.
    assert lidarr_get({"lidarr_eventtype": "Test"}, "Lidarr_EventType") == "Test"


def test_lidarr_get_prefers_exact_over_case_fold():
    env = {"lidarr_sourcepath": "/lower", "Lidarr_SourcePath": "/exact"}
    assert lidarr_get(env, "Lidarr_SourcePath") == "/exact"


def test_lidarr_get_default_when_absent():
    assert lidarr_get({}, "Lidarr_Album_Id", "fallback") == "fallback"
