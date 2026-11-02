#!/bin/sh
# Live-patch WindowServer's show_shield_windows (see ~/Work/windowserver.md §5). Run as root.
# Usage: ws-shield-fix.sh [check|apply]   (default: apply)
# The patch lives in WindowServer's memory; LaunchDaemon com.wenyuan.ws-shield-fix
# (ws-shield-watch, built from ws-shield-watch.rs) re-applies it to each new WindowServer after boot/logout.
set -eu
DIR=$(cd "$(dirname "$0")" && pwd)
MODE=${1:-apply}
BUILD=$(sw_vers -buildVersion)
[ "$BUILD" = 25F84 ] || { echo "build $BUILD is not 25F84; patch offsets do not apply"; exit 1; }
PID=$(pgrep -x WindowServer)
date
/usr/bin/lldb --batch --no-lldbinit \
  -o 'settings set target.preload-symbols false' \
  -o 'settings set symbols.enable-external-lookup false' \
  -o "process attach --pid $PID" \
  -o "command script import $DIR/shield_fix.py" \
  -o "shieldfix $MODE" \
  -o 'process detach'
date
