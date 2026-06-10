#!/bin/sh
# OnReleaseImport custom script: runs the REAL musefs-lidarr-sync inside the
# Lidarr container (bind-mounted source + apk python3). It queries Lidarr's API
# for the imported album's metadata and writes those tags into the shared musefs
# store. Autoscan is off — the host pre-registers the track so the store key
# (the backing realpath) lines up; downloads/library are bind-mounted at
# identical host/container paths so realpaths match.
# shellcheck source=/dev/null
[ -f /musefs/bin/env.sh ] && . /musefs/bin/env.sh
export MUSEFS_DB=/musefs/store/store.db
export MUSEFS_LIDARR_URL=http://localhost:8686
export MUSEFS_LIDARR_AUTOSCAN=0
export MUSEFS_LIDARR_LINK_MODE=symlink
exec python3 -c "import sys; sys.path[:0] = ['/musefs/lidarr-src', '/musefs/common-src']; from musefs_lidarr.cli_sync import main; sys.exit(main())"
