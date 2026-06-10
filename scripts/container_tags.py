"""Compute the GHCR image references for a musefs container release.

The release workflow publishes two multi-arch manifests per tag: glibc
(``:VERSION`` / ``:latest``) and musl (``:VERSION-musl`` / ``:musl``). The
floating tags (``latest`` / ``musl``) move only on stable releases; a prerelease
tag whose version carries a ``-`` segment (e.g. ``v0.2.0-rc1``) publishes only
the immutable version-pinned tags. GHCR rejects uppercase in image references,
so the owner is lowercased here (the GitHub owner is mixed-case ``Sohex``).
"""

from __future__ import annotations

import argparse

_VARIANTS = {
    "glibc": ("", "latest"),
    "musl": ("-musl", "musl"),
}


def registry_ref(repo: str) -> str:
    """Return the lowercased GHCR image path for ``owner/name`` ``repo``."""
    return f"ghcr.io/{repo}".lower()


def version_from_ref(ref: str) -> str:
    """Strip a leading ``v`` from a tag ref (``v0.2.0`` -> ``0.2.0``)."""
    return ref[1:] if ref.startswith("v") else ref


def is_prerelease(version: str) -> bool:
    """A version is a prerelease iff it carries a ``-`` pre-release segment."""
    return "-" in version


def tags_for(repo: str, ref: str, variant: str) -> list[str]:
    """Full image refs to publish for ``variant`` at tag ``ref``."""
    if variant not in _VARIANTS:
        raise ValueError(f"unknown variant: {variant!r}")
    suffix, floating = _VARIANTS[variant]
    base = registry_ref(repo)
    version = version_from_ref(ref)
    tags = [f"{base}:{version}{suffix}"]
    if not is_prerelease(version):
        tags.append(f"{base}:{floating}")
    return tags


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="owner/name, e.g. Sohex/musefs")
    parser.add_argument("--ref", required=True, help="tag ref, e.g. v0.2.0")
    parser.add_argument("--variant", required=True, choices=sorted(_VARIANTS))
    args = parser.parse_args(argv)
    for tag in tags_for(args.repo, args.ref, args.variant):
        print(tag)
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
