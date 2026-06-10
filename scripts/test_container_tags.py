import pytest
from container_tags import (
    is_prerelease,
    main,
    registry_ref,
    tags_for,
    version_from_ref,
)


def test_registry_ref_lowercases_owner():
    # GitHub owner is mixed-case "Sohex"; GHCR rejects uppercase refs.
    assert registry_ref("Sohex/musefs") == "ghcr.io/sohex/musefs"


def test_version_from_ref_strips_leading_v():
    assert version_from_ref("v0.2.0") == "0.2.0"
    assert version_from_ref("0.2.0") == "0.2.0"


def test_is_prerelease():
    assert is_prerelease("0.2.0") is False
    assert is_prerelease("0.2.0-rc1") is True


def test_tags_for_glibc_stable():
    assert tags_for("Sohex/musefs", "v0.2.0", "glibc") == [
        "ghcr.io/sohex/musefs:0.2.0",
        "ghcr.io/sohex/musefs:latest",
    ]


def test_tags_for_musl_stable():
    assert tags_for("Sohex/musefs", "v0.2.0", "musl") == [
        "ghcr.io/sohex/musefs:0.2.0-musl",
        "ghcr.io/sohex/musefs:musl",
    ]


def test_tags_for_glibc_prerelease_omits_latest():
    assert tags_for("Sohex/musefs", "v0.2.0-rc1", "glibc") == [
        "ghcr.io/sohex/musefs:0.2.0-rc1",
    ]


def test_tags_for_musl_prerelease_omits_floating():
    assert tags_for("Sohex/musefs", "v0.2.0-rc1", "musl") == [
        "ghcr.io/sohex/musefs:0.2.0-rc1-musl",
    ]


def test_unknown_variant_raises():
    with pytest.raises(ValueError):
        tags_for("Sohex/musefs", "v0.2.0", "windows")


def test_main_prints_newline_separated(capsys):
    rc = main(["--repo", "Sohex/musefs", "--ref", "v0.2.0", "--variant", "glibc"])
    out = capsys.readouterr().out
    assert rc == 0
    assert out == "ghcr.io/sohex/musefs:0.2.0\nghcr.io/sohex/musefs:latest\n"
