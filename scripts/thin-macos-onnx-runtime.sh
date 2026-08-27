#!/bin/sh

set -eu

app=${1:-}
if [ -z "$app" ]; then
  echo "Usage: $0 /path/to/Dakia.app" >&2
  exit 2
fi
if [ ! -d "$app" ] || [ -L "$app" ]; then
  echo "Missing or unsafe packaged Dakia app bundle: $app" >&2
  exit 1
fi

# Cargo's ONNX Runtime framework is universal even for an Apple Silicon app.
# Trim only the copy inside the completed bundle, then let the caller's final
# signing pass sign the modified framework and enclosing app.
runtime="$app/Contents/Frameworks/libonnxruntime.1.23.2.dylib"
framework_dir=$(dirname "$runtime")
if [ ! -f "$runtime" ] || [ -L "$runtime" ]; then
  echo "Missing or unsafe packaged ONNX Runtime framework: $runtime" >&2
  exit 1
fi
if ! runtime_archs=$(lipo -archs "$runtime"); then
  echo "Cannot inspect packaged ONNX Runtime framework architecture: $runtime" >&2
  exit 1
fi
require_runtime_install_name() {
  candidate=$1
  install_name=$(otool -D "$candidate" | sed -n '2p' | xargs)
  if [ "$install_name" != "@rpath/libonnxruntime.1.23.2.dylib" ]; then
    echo "Packaged ONNX Runtime framework has an unexpected install name: $install_name" >&2
    exit 1
  fi
}

case "$runtime_archs" in
  arm64)
    require_runtime_install_name "$runtime"
    ;;
  "x86_64 arm64"|"arm64 x86_64")
    # The temporary output must share the framework filesystem so the final
    # move cannot degrade into a cross-volume copy.
    thin_dir=$(mktemp -d "$framework_dir/.dakia-onnx-thin.XXXXXX")
    thin_runtime="$thin_dir/libonnxruntime.1.23.2.dylib"
    cleanup() {
      rm -rf "$thin_dir"
    }
    trap cleanup EXIT HUP INT TERM
    if ! lipo "$runtime" -thin arm64 -output "$thin_runtime"; then
      echo "Failed to thin the packaged ONNX Runtime framework to arm64." >&2
      exit 1
    fi
    if [ "$(lipo -archs "$thin_runtime")" != "arm64" ]; then
      echo "Failed to thin the packaged ONNX Runtime framework to exactly arm64." >&2
      exit 1
    fi
    require_runtime_install_name "$thin_runtime"
    mv "$thin_runtime" "$runtime"
    rm -rf "$thin_dir"
    trap - EXIT HUP INT TERM
    ;;
  *)
    echo "Packaged ONNX Runtime framework has unsupported architecture set: $runtime_archs" >&2
    exit 1
    ;;
esac
