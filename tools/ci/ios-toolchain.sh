#!/bin/bash
# Fail before compiling Rust when the selected Xcode cannot build the native launcher.
set -euo pipefail
if ! version=$(xcodebuild -version); then
  echo '::error::Could not run xcodebuild. Select a full Xcode installation with DEVELOPER_DIR or xcode-select.' >&2
  exit 1
fi
printf '%s\n' "$version"
major=$(printf '%s\n' "$version" | sed -nE 's/^Xcode ([0-9]+).*/\1/p')
if ! [[ "$major" =~ ^[0-9]+$ ]] || (( major < 26 )); then
  echo '::error::The native iOS launcher requires Xcode 26 or newer for Liquid Glass. Use macos-26, or select Xcode 26+ on the MAC_RUNNER override.' >&2
  exit 1
fi
if ! sdk=$(xcrun --sdk iphoneos --show-sdk-version); then
  echo '::error::Could not locate the iPhoneOS SDK in the selected Xcode installation.' >&2
  exit 1
fi
printf 'iPhoneOS SDK: %s\n' "$sdk"
major=${sdk%%.*}
if ! [[ "$major" =~ ^[0-9]+$ ]] || (( major < 26 )); then
  echo '::error::The selected iPhoneOS SDK must be 26 or newer. Check DEVELOPER_DIR and xcode-select.' >&2
  exit 1
fi
for compiler in swiftc clang; do
  if ! xcrun --sdk iphoneos --find "$compiler" >/dev/null; then
    echo "::error::The selected Xcode does not provide $compiler for iPhoneOS. Install or select a complete Xcode toolchain." >&2
    exit 1
  fi
done
