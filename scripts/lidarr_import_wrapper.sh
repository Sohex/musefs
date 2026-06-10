#!/bin/sh
# Bind-mounted into the Lidarr container by scripts/lidarr-smoke.sh and set as
# the Custom Script path. Lidarr execs this on the Test event; it runs the REAL
# musefs-lidarr-import from the bind-mounted source using the container's
# python3 (apk-installed by the smoke). A success return proves real Lidarr
# execs the real script and that lidarr_get resolves Lidarr's lowercased env
# keys end to end (the StringDictionary case bug the 2026-06-07 run caught).
exec python3 -c "import sys; sys.path[:0] = ['/musefs/lidarr-src', '/musefs/common-src']; from musefs_lidarr.cli_import import main; sys.exit(main())"
