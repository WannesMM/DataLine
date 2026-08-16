#!/usr/bin/env bash
# Rebuilds DataLineFFI.xcframework and the generated Swift bindings from the
# current dataline-ffi/dataline-core Rust source.
#
# This automates the 5-step manual process Architecture.md's Implementation
# notes describe in prose (per-arch release build, uniffi-bindgen codegen,
# lipo, xcodebuild -create-xcframework, copy the generated .swift file over)
# — previously undocumented as an actual script, which Architecture.md §9
# Testing Strategy names as the direct cause of one real bug already (a
# stale generated C header from a partially-redone-by-hand rebuild).
#
# Usage: scripts/build-xcframework.sh

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$(pwd)"

export MACOSX_DEPLOYMENT_TARGET=14.0

echo "==> Building dataline-ffi (release) for aarch64-apple-darwin"
cargo build --release -p dataline-ffi --target aarch64-apple-darwin

echo "==> Building dataline-ffi (release) for x86_64-apple-darwin"
cargo build --release -p dataline-ffi --target x86_64-apple-darwin

echo "==> Generating Swift bindings"
rm -rf "$ROOT/dataline-ffi/bindings"
mkdir -p "$ROOT/dataline-ffi/bindings"
cargo run -p dataline-ffi --bin uniffi-bindgen -- generate \
    --library "$ROOT/target/aarch64-apple-darwin/release/libdataline_ffi.dylib" \
    --language swift \
    --out-dir "$ROOT/dataline-ffi/bindings"

echo "==> Creating universal (arm64 + x86_64) static library"
mkdir -p "$ROOT/xcframework/universal-macos"
lipo -create \
    "$ROOT/target/aarch64-apple-darwin/release/libdataline_ffi.a" \
    "$ROOT/target/x86_64-apple-darwin/release/libdataline_ffi.a" \
    -output "$ROOT/xcframework/universal-macos/libdataline_ffi.a"

echo "==> Refreshing headers"
mkdir -p "$ROOT/xcframework/headers"
cp "$ROOT/dataline-ffi/bindings/dataline_ffiFFI.h" "$ROOT/xcframework/headers/dataline_ffiFFI.h"
cp "$ROOT/dataline-ffi/bindings/dataline_ffiFFI.modulemap" "$ROOT/xcframework/headers/module.modulemap"

echo "==> Rebuilding DataLineFFI.xcframework"
rm -rf "$ROOT/xcframework/DataLineFFI.xcframework"
xcodebuild -create-xcframework \
    -library "$ROOT/xcframework/universal-macos/libdataline_ffi.a" \
    -headers "$ROOT/xcframework/headers" \
    -output "$ROOT/xcframework/DataLineFFI.xcframework"

echo "==> Copying generated Swift bindings into the Swift package"
cp "$ROOT/dataline-ffi/bindings/dataline_ffi.swift" "$ROOT/xcframework/Sources/DataLineFFI/DataLineFFI.swift"

echo "==> Done. Verify with: (cd xcframework && swift run DataLineSmokeTest)"
