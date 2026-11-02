#!/bin/sh
set -eu
SRC=$(cd "$(dirname "$0")/.." && pwd)
TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ws-shield-test.XXXXXX")
trap 'rm -rf "$TEST_DIR"' EXIT HUP INT TERM
rustc -O --edition 2021 -D warnings --test -o "$TEST_DIR/tests" "$SRC/ws-shield-watch.rs"
/usr/bin/codesign --force --sign - --entitlements "$SRC/tests/entitlements.plist" "$TEST_DIR/tests"
"$TEST_DIR/tests" --test-threads=1 "$@"
