#!/bin/bash
# Resolve the release executable using Cargo's effective target directory.
set -euo pipefail
: "${1:?expected an iOS target}"
cargo metadata --format-version 1 --no-deps --locked | python3 -c '
import json
from pathlib import Path
import sys

metadata = json.load(sys.stdin)
print(Path(metadata["target_directory"]) / sys.argv[1] / "release" / "horizon-official")
' "$1"
