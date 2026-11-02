#!/bin/sh
# Install com.wenyuan.ws-shield-fix: auto-apply the shield-leak patch to each new WindowServer. Run as root.
# Build the watcher first (as your user): ./build.sh
# Remove: launchctl bootout system/com.wenyuan.ws-shield-fix; rm /Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist; rm -r /usr/local/libexec/ws-shield-fix
set -eu
SRC=$(cd "$(dirname "$0")" && pwd)
DEST=/usr/local/libexec/ws-shield-fix
PLIST=/Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist
[ "$(id -u)" = 0 ] || { echo 'run with sudo after ./build.sh' >&2; exit 1; }
[ "$(uname -sm)" = 'Darwin arm64' ] || { echo 'requires macOS on Apple Silicon' >&2; exit 1; }
[ "$(sw_vers -buildVersion)" = 25F84 ] || { echo 'requires macOS build 25F84' >&2; exit 1; }
for FILE in ws-shield-watch ws-shield-watch.rs shield_patch.rs com.wenyuan.ws-shield-fix.plist; do
    [ -r "$SRC/$FILE" ] || { echo "missing $FILE; run ./build.sh first" >&2; exit 1; }
done
[ -x "$SRC/ws-shield-watch" ] && [ ! "$SRC/ws-shield-watch.rs" -nt "$SRC/ws-shield-watch" ] && [ ! "$SRC/shield_patch.rs" -nt "$SRC/ws-shield-watch" ] || {
    echo 'run ./build.sh to build the current watcher source' >&2; exit 1;
}
/usr/bin/lipo "$SRC/ws-shield-watch" -verify_arch arm64
/usr/bin/codesign --verify "$SRC/ws-shield-watch"
/usr/bin/plutil -lint "$SRC/com.wenyuan.ws-shield-fix.plist"
if launchctl print system/com.wenyuan.ws-shield-fix >/dev/null 2>&1; then
    launchctl bootout system "$PLIST"
fi
install -d -o root -g wheel -m 755 "$DEST"
install -o root -g wheel -m 755 "$SRC/ws-shield-watch" "$DEST/"
rm -f "$DEST/ws-shield-watch.sh" "$DEST/ws-shield-watch.py" "$DEST/ws-shield-fix.sh" "$DEST/shield_fix.py"
install -o root -g wheel -m 644 "$SRC/com.wenyuan.ws-shield-fix.plist" "$PLIST"
launchctl bootstrap system "$PLIST"
echo "installed $PLIST"
