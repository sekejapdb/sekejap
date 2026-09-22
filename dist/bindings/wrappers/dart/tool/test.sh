#!/usr/bin/env bash
# Run the wrapper's tests against a libsekejap that is already built.
#
#   tool/test.sh /path/to/libsekejap.dylib
#
# With no argument, SEKEJAP_LIBRARY is used as it stands.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ge 1 ]; then
  export SEKEJAP_LIBRARY="$1"
  shift
fi

if [ -z "${SEKEJAP_LIBRARY:-}" ]; then
  echo "Set SEKEJAP_LIBRARY, or pass the library path as the first argument." >&2
  echo "Build it with: cargo build --release -p sekejap-capi" >&2
  exit 2
fi

dart pub get
dart analyze
dart test "$@"
