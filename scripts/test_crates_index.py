from crates_index import index_path, is_published


def test_index_path_short_names():
    assert index_path("a") == "1/a"
    assert index_path("ab") == "2/ab"
    assert index_path("abc") == "3/a/abc"


def test_index_path_long_name():
    assert index_path("musefs-db") == "mu/se/musefs-db"
    assert index_path("musefs") == "mu/se/musefs"


def test_is_published_true_when_version_present():
    body = '{"name":"musefs-db","vers":"0.2.0"}\n{"name":"musefs-db","vers":"1.0.0"}\n'
    assert is_published("musefs-db", "1.0.0", fetch=lambda url: body) is True


def test_is_published_false_when_version_absent():
    body = '{"name":"musefs-db","vers":"0.2.0"}\n'
    assert is_published("musefs-db", "1.0.0", fetch=lambda url: body) is False


def test_is_published_false_when_crate_missing():
    def fetch_404(url):
        raise FileNotFoundError(url)

    assert is_published("musefs-db", "1.0.0", fetch=fetch_404) is False


def test_is_published_uses_correct_url():
    seen = {}

    def fetch(url):
        seen["url"] = url
        return '{"vers":"1.0.0"}\n'

    is_published("musefs-db", "1.0.0", fetch=fetch)
    assert seen["url"] == "https://index.crates.io/mu/se/musefs-db"
