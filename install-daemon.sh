#!/bin/sh
# Install com.wenyuan.ws-shield-fix: auto-apply the shield-leak patch to each new WindowServer. Run as root.
# Build the watcher first (as your user): rustc -O --edition 2021 -o ws-shield-watch ws-shield-watch.rs
# Remove: launchctl bootout system/com.wenyuan.ws-shield-fix; rm /Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist; rm -r /usr/local/libexec/ws-shield-fix
set -eu
SRC=$(cd "$(dirname "$0")" && pwd)
DEST=/usr/local/libexec/ws-shield-fix
PLIST=/Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist
install -d -o root -g wheel -m 755 "$DEST"
install -o root -g wheel -m 755 "$SRC/ws-shield-fix.sh" "$SRC/ws-shield-watch" "$DEST/"
install -o root -g wheel -m 644 "$SRC/shield_fix.py" "$DEST/"
rm -f "$DEST/ws-shield-watch.sh" "$DEST/ws-shield-watch.py"
install -o root -g wheel -m 644 "$SRC/com.wenyuan.ws-shield-fix.plist" "$PLIST"
launchctl bootout system "$PLIST" 2>/dev/null || true
launchctl bootstrap system "$PLIST"
echo "installed $PLIST"
