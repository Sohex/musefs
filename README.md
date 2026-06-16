<p align="center">
  <img src="assets/banner.svg" alt="musefs — tag your library without duplicating a single byte" width="100%">
</p>

# musefs

[![CI](https://github.com/Sohex/musefs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sohex/musefs/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/Sohex/musefs/branch/main/graph/badge.svg)](https://codecov.io/gh/Sohex/musefs)
[![Release](https://img.shields.io/github/v/release/Sohex/musefs?sort=semver)](https://github.com/Sohex/musefs/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A read-only FUSE filesystem that presents a re-tagged, reorganized view of
your music library — without modifying or duplicating a single byte of the
original audio. Fix tags, art, and folder structure in a SQLite store; the
mount shows a clean library while your files stay exactly as they are.

Because the mount never rewrites a byte, every backing file keeps its exact
checksum forever. That suits two jobs the usual "edit tags in place" tools
can't: **archival**, where you retag and reorganize the presented library as
much as you like while the originals stay bit-for-bit verifiable; and
**torrents**, where you keep seeding from the untouched files while serving a
clean, re-tagged view of them on top.

Runs on **Linux**, **FreeBSD**, and **macOS\*** &mdash; *\*macOS builds and
passes unit tests, but mounted end-to-end behaviour is not yet validated.*

## Quick start

```bash
cargo install musefs    # compiles from source — needs a Rust toolchain,
                        # libfuse3-dev and pkg-config; prebuilt binaries
                        # and container images: see Installing

musefs scan ~/Music --db library.db        # ingest your library
mkdir -p ~/mnt/music
musefs mount ~/mnt/music --db library.db \
    --template '$albumartist/$album/$title'
# mount blocks until unmounted: fusermount -u ~/mnt/music (or Ctrl-C)
```

`~/mnt/music` now serves your library as
`Album Artist/Album/Title.flac` — with each file's metadata generated fresh
from the database, spliced in front of your original, untouched audio.

See [Installation](https://sohex.github.io/musefs/guide/installation.html) for prebuilt binaries, container images, and platform notes.

## Documentation

Full documentation lives at **<https://sohex.github.io/musefs/>**:

- [Installation](https://sohex.github.io/musefs/guide/installation.html) ·
  [Scanning](https://sohex.github.io/musefs/guide/scanning.html) ·
  [Mounting & path templates](https://sohex.github.io/musefs/guide/mounting.html) ·
  [Tuning](https://sohex.github.io/musefs/guide/tuning.html) ·
  [FAQ](https://sohex.github.io/musefs/guide/faq.html)
- [Supported formats](https://sohex.github.io/musefs/formats/overview.html)
- [Integrations](https://sohex.github.io/musefs/integrations/overview.html):
  [beets](https://sohex.github.io/musefs/integrations/beets.html) ·
  [Picard](https://sohex.github.io/musefs/integrations/picard.html) ·
  [Lidarr](https://sohex.github.io/musefs/integrations/lidarr.html) ·
  [systemd](https://sohex.github.io/musefs/integrations/systemd.html) ·
  [python-musefs](https://sohex.github.io/musefs/integrations/python-musefs.html)
- [Architecture](https://sohex.github.io/musefs/architecture/overview.html) ·
  [Contributing](https://sohex.github.io/musefs/contributing/setup.html) ·
  [Benchmarks](https://sohex.github.io/musefs/benchmarks.html) ·
  [Changelog](https://sohex.github.io/musefs/changelog.html)

## License

Licensed under the [MIT License](LICENSE).

## Acknowledgements

The Lidarr real-instance end-to-end test plays a real album through a real
Lidarr. With thanks to **[Komiku](https://loyaltyfreakmusic.com/)** (Loyalty
Freak Music), whose track *"The calling"* (from *The Adventure Goes On, Vol. 1*)
is dedicated to the public domain under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/)
and vendored as that test's fixture — thank you for releasing music freely.

## AI Disclaimer

This project was developed using AI, primarily Claude Opus and MiMo v2.5.
