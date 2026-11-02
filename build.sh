#!/bin/sh
# Build the watcher as the current user before installing the daemon.
set -eu
SRC=$(cd "$(dirname "$0")" && pwd)
[ "$(uname -sm)" = 'Darwin arm64' ] || { echo 'requires macOS on Apple Silicon' >&2; exit 1; }
BUILD_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ws-shield-build.XXXXXX")
trap 'rm -rf "$BUILD_DIR"' EXIT HUP INT TERM
rustc -O --edition 2021 -D warnings -o "$BUILD_DIR/ws-shield-watch" "$SRC/ws-shield-watch.rs"
/usr/bin/codesign --force --sign - --entitlements "$SRC/debugger.entitlements" "$BUILD_DIR/ws-shield-watch"
mv "$BUILD_DIR/ws-shield-watch" "$SRC/ws-shield-watch"
echo "built $SRC/ws-shield-watch"
