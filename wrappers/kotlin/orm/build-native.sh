#!/usr/bin/env bash
# Cross-compile the sekejap JNI core (../rust) for Android ABIs and the host.
#
#   ./build-native.sh android    # arm64-v8a, armeabi-v7a, x86_64 → jniLibs/
#   ./build-native.sh host       # this machine → ../rust/target/release (for tests)
#   ./build-native.sh all
#
# Requires: rustup targets, cargo-ndk, and ANDROID_NDK_HOME (or NDK auto-detect).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RUST="$HERE/../rust"
JNILIBS="${JNILIBS:-$HERE/jniLibs}"

build_android() {
  : "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to your NDK (e.g. \$ANDROID_HOME/ndk/<v>)}"
  ( cd "$RUST" && cargo ndk \
      -t arm64-v8a -t armeabi-v7a -t x86_64 \
      -o "$JNILIBS" build --release )
  echo "Android .so → $JNILIBS/<abi>/libsekejap_jni.so"
}

build_host() {
  ( cd "$RUST" && cargo build --release )
  echo "Host lib → $RUST/target/release/libsekejap_jni.{dylib,so,dll}"
}

case "${1:-all}" in
  android) build_android ;;
  host)    build_host ;;
  all)     build_host; build_android ;;
  *) echo "usage: $0 {android|host|all}"; exit 1 ;;
esac
