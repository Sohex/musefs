# Quick start

```bash
cargo install musefs    # compiles from source — needs a Rust toolchain,
                        # libfuse3-dev and pkg-config; prebuilt binaries
                        # and container images: see Installing

musefs scan ~/Music --db library.db        # ingest your library
mkdir -p ~/mnt/music
musefs mount ~/mnt/music --db library.db \
    --template '$albumartist/$album/$title'
# mount blocks until unmounted: fusermount3 -u ~/mnt/music (or Ctrl-C)
```

`~/mnt/music` now serves your library as
`Album Artist/Album/Title.flac` — with each file's metadata generated fresh
from the database, spliced in front of your original, untouched audio.
