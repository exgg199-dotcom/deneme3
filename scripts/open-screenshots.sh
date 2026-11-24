#!/usr/bin/env bash
# Open screenshots already downloaded from a workflow that produces them.
cd "$(git rev-parse --show-toplevel)/ci-artifacts/latest" || exit 1
for dir in */linux-screenshots */ios-screenshots; do
  [ -d "$dir" ] || continue
  if command -v xdg-open >/dev/null; then xdg-open "$dir"; elif command -v open >/dev/null; then open "$dir"; else echo "$PWD/$dir"; fi
done
