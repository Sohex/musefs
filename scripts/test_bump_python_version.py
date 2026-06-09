import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bump_python_version as bp  # noqa: E402


def test_set_project_version_replaces_first_only():
    text = '[project]\nname = "x"\nversion = "0.1.0"\n'
    assert bp.set_project_version(text, "0.2.0") == '[project]\nname = "x"\nversion = "0.2.0"\n'


def test_set_project_version_missing_raises():
    with pytest.raises(ValueError):
        bp.set_project_version('[project]\nname = "x"\n', "0.2.0")


def test_set_init_version():
    assert bp.set_init_version('__version__ = "0.1.0"\n', "0.2.0") == '__version__ = "0.2.0"\n'


def test_set_dep_floor_keeps_siblings():
    text = 'dependencies = ["python-musefs>=0.1.0", "beets>=1.6"]'
    assert (
        bp.set_dep_floor(text, "0.2.0") == 'dependencies = ["python-musefs>=0.2.0", "beets>=1.6"]'
    )


def test_bump_rewrites_tree(tmp_path):
    pyproject_beets = (
        '[project]\nversion = "0.1.0"\ndependencies = ["python-musefs>=0.1.0", "beets>=1.6"]\n'
    )
    pyproject_lidarr = '[project]\nversion = "0.1.0"\ndependencies = ["python-musefs>=0.1.0"]\n'
    files = {
        "contrib/python-musefs/pyproject.toml": (
            '[project]\nname = "python-musefs"\nversion = "0.1.0"\n'
        ),
        "contrib/beets/pyproject.toml": pyproject_beets,
        "contrib/lidarr/pyproject.toml": pyproject_lidarr,
        "contrib/picard/pyproject.toml": ('[project]\nname = "musefs-picard"\nversion = "0.1.0"\n'),
        "contrib/python-musefs/src/musefs_common/__init__.py": ('__version__ = "0.1.0"\n'),
        "contrib/lidarr/src/musefs_lidarr/__init__.py": ('__version__ = "0.1.0"\n'),
    }
    for rel, content in files.items():
        p = tmp_path / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)

    bp.bump("0.2.0", root=tmp_path, run_vendor=False)

    for rel in bp.PYPROJECTS:
        assert 'version = "0.2.0"' in (tmp_path / rel).read_text()
    for rel in bp.INIT_FILES:
        assert '__version__ = "0.2.0"' in (tmp_path / rel).read_text()
    for rel in bp.DEPENDENTS:
        assert "python-musefs>=0.2.0" in (tmp_path / rel).read_text()


@pytest.mark.parametrize(
    "version", ["0.2.0", "1.2.3", "1.2.3a1", "1.2.3rc1", "1.2.3.post1", "1.2.3.dev1"]
)
def test_is_valid_version_accepts(version):
    assert bp.is_valid_version(version)


@pytest.mark.parametrize("version", ["", "v1.2.3", "1.2.3-beta", "not a version"])
def test_is_valid_version_rejects(version):
    assert not bp.is_valid_version(version)


def test_main_rejects_bad_version(capsys):
    assert bp.main(["not a version"]) == 2
