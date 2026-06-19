from musefs_lidarr import __version__
from musefs_lidarr.errors import MusefsLidarrError


def test_package_imports():
    assert __version__ == "1.2.0"
    assert str(MusefsLidarrError("problem")) == "problem"
