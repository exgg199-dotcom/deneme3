#!/usr/bin/env bash
# Download the newest successful CI run's artifacts for a branch into ./ci-artifacts/<run-id>.<artifact-id-or-attempt>/<artifact>/, and point
# ./ci-artifacts/latest/<workflow> at it. Needs the GitHub CLI, signed in.
#   scripts/fetch-artifacts.sh                 # this branch; ios
#   scripts/fetch-artifacts.sh main ios        # another branch, other workflows
set -euo pipefail
repo=${REPO:-Kelly-Inc/horizon-ios}
branch=${1:-$(git rev-parse --abbrev-ref HEAD)}
shift || true
workflows=${*:-ios}
cd "$(git rev-parse --show-toplevel)"
mkdir -p ci-artifacts/latest
has_ipa() {
  local ipa="$1/$artifact/Horizon-game-unsigned.ipa"
  [ -f "$ipa" ] && [ -s "$ipa" ]
}
cache_complete() {
  [ -f "$dir/.complete" ] && { [ -z "$artifact" ] || has_ipa "$dir"; }
}
download_run() (
  stage=$(mktemp -d "ci-artifacts/.download-$cache.XXXXXXXX") || exit 1
  trap 'rm -r -- "$stage"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  if [ -n "$artifact" ]; then
    gh run download "$id" -R "$repo" -D "$stage/$cache/$artifact" --name "$artifact" || exit 1
  else
    gh run download "$id" -R "$repo" -D "$stage/$cache" || exit 1
  fi
  if [ -n "$artifact" ] && ! has_ipa "$stage/$cache"; then
    echo "ios: artifact $artifact has no Horizon-game-unsigned.ipa" >&2
    exit 1
  fi
  # Unpack before publishing so a failed extraction also leaves the cache untouched.
  for tarball in "$stage/$cache"/*/*.tar.gz; do
    if [ -f "$tarball" ]; then tar -xzf "$tarball" -C "$(dirname "$tarball")" || exit 1; fi
  done
  touch "$stage/$cache/.complete" || exit 1
  if cache_complete; then return 0; fi
  if [ -e "$dir" ]; then
    # A pre-upgrade cache cannot prove its download finished. Keep its contents.
    backup=$(mktemp -d "ci-artifacts/.unverified-$cache.XXXXXXXX") || exit 1
    mv "$dir" "$backup/" || exit 1
  fi
  # Use the parent as the destination. A concurrent successful run already at
  # ci-artifacts/$cache wins instead of receiving a nested staging directory.
  if ! mv "$stage/$cache" ci-artifacts/; then
    cache_complete
  fi
)
status=0
for wf in $workflows; do
  run=$(gh run list -R "$repo" --workflow "$wf.yml" --branch "$branch" --status success --limit 1 \
    --json databaseId,attempt --jq '.[0] | select(. != null) | "\(.databaseId) \(.attempt)"')
  if [ -z "$run" ]; then
    echo "$wf: no successful run on $branch" >&2
    status=1
    continue
  fi
  id=${run%% *}
  cache="$id.${run#* }"
  artifact=
  if [ "$wf" = ios ]; then
    # A publishing-only rerun can reuse an IPA from an earlier attempt.
    # Pick the newest available IPA, and key its cache by artifact identity.
    selected=$(gh api --paginate "repos/$repo/actions/runs/$id/artifacts?per_page=100" \
      --jq '.artifacts[] | select(.expired == false and (.name | test("^horizon-ios-game(-[1-9][0-9]*)?$"))) | [.id,.name] | @tsv' \
      | sort -k1,1n | tail -1)
    if [ -z "$selected" ]; then
      echo "ios: no available IPA artifact for run $id" >&2
      status=1
      continue
    fi
    cache="$id.${selected%%$'\t'*}"
    artifact=${selected#*$'\t'}
  fi
  latest="ci-artifacts/latest/$wf"
  # PowerShell can leave a real directory here. ln would put the link inside it.
  if [ -d "$latest" ] && [ ! -L "$latest" ]; then
    echo "$wf: cannot update $latest because it is a directory" >&2
    status=1
    continue
  fi
  dir="ci-artifacts/$cache"
  if ! cache_complete && ! download_run; then
    echo "$wf: could not cache artifacts from run $id" >&2
    status=1
    continue
  fi
  if ! ln -sfn "../$cache" "$latest"; then
    status=1
    continue
  fi
  echo "$wf: run $id -> $latest"
done
exit "$status"
