#!/bin/sh
# Lidarr "Use Script Import" handler: runs the REAL musefs-lidarr-import inside
# the container to symlink the imported library path to the untouched backing
# download (so musefs serves the original bytes). Lidarr passes Lidarr_SourcePath
# and Lidarr_DestinationPath in the environment.
# shellcheck source=/dev/null
[ -f /musefs/bin/env.sh ] && . /musefs/bin/env.sh
export MUSEFS_LIDARR_LINK_MODE=symlink
exec python3 -c "import sys; sys.path[:0] = ['/musefs/lidarr-src', '/musefs/common-src']; from musefs_lidarr.cli_import import main; sys.exit(main())"
