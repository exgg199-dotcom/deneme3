#!/usr/bin/env bash
# Called only by the trusted main-branch release job, after auditing its artifact.
set -euo pipefail
: "${GH_REPO:?}" "${GITHUB_SHA:?}" "${IPA:?}" "${CHECKSUM:?}" "${VERSION:?}" "${BUILD:?}"
tag="ios-build-${BUILD}"
ipa_name=$(basename "$IPA")
checksum_name=$(basename "$CHECKSUM")
python3 - "$IPA" "$CHECKSUM" <<'PYCHECKSUM'
import hashlib
import sys
from pathlib import Path

ipa, checksum = map(Path, sys.argv[1:])
with ipa.open("rb") as stream:
    digest = hashlib.file_digest(stream, "sha256").hexdigest()
if checksum.read_bytes() != f"{digest}  {ipa.name}\n".encode("utf-8"):
    raise SystemExit("Checksum does not match the IPA")
PYCHECKSUM
# An existing release's tag determines its commit. A commit lookup with an
# explicit tag ref also works when a branch happens to have the same name.
check_release_tag() {
  local refs target
  refs=$(gh api "repos/$GH_REPO/git/matching-refs/tags/$tag" --jq '.[].ref')
  tag_exists=false
  if grep -Fx -- "refs/tags/$tag" <<< "$refs" >/dev/null; then
    target=$(gh api "repos/$GH_REPO/commits/tags/$tag" --jq .sha)
    if [[ "$target" != "$GITHUB_SHA" ]]; then
      echo '::error::Release tag points to a different commit; refusing to modify it.'
      exit 1
    fi
    tag_exists=true
  fi
}
check_release_tag
had_tag=$tag_exists
# An older run retried after a newer release must not take over Latest.
latest=true
if gh api "repos/$GH_REPO/releases/latest" --include > release-latest.http; then
  latest_tag=$(python3 - <<'PYRESPONSE'
import json
_, separator, body = open("release-latest.http", encoding="utf-8").read().partition("\n\n")
tag = json.loads(body)["tag_name"] if separator else None
if not isinstance(tag, str) or not tag:
    raise SystemExit("Invalid Latest release response")
print(tag)
PYRESPONSE
  )
  latest=$(python3 - "$latest_tag" "$BUILD" "$VERSION" <<'PYLATEST'
import re, sys
match = re.fullmatch(r"ios-build-([0-9]+)\.([0-9]+)", sys.argv[1])
current = tuple(map(int, sys.argv[2].split(".")))
local_version = re.fullmatch(r"v?([0-9]+)\.([0-9]+)\.([0-9]+)", sys.argv[1])
if local_version:
    newer = tuple(map(int, sys.argv[3].split("."))) > tuple(map(int, local_version.groups()))
else:
    newer = not match or current >= tuple(map(int, match.groups()))
print(str(newer).lower())
PYLATEST
  )
elif ! [[ "$(head -n 1 release-latest.http)" =~ ^HTTP/[^[:space:]]+[[:space:]]+404([[:space:]]|$) ]]; then
  echo '::error::Could not determine the Latest release; refusing to publish.'
  exit 1
fi
# Keep notes/title current even when a prior attempt already published this tag.
{
  printf '%s\n\n' '🤖 Agent: Automated unsigned iOS build from GitHub Actions.'
  printf 'Horizon %s, build %s, from commit %s.\n\n' "$VERSION" "$BUILD" "$GITHUB_SHA"
  printf '%s\n\n' 'Download the unsigned IPA below and sign it with your own signing service or certificate before installing it on an iPhone or iPad. No signing certificate, provisioning profile, account data, or prepared Minecraft assets are included.'
  printf '%s\n' 'The adjacent .sha256 file verifies the downloaded IPA.'
} > release-notes.md
uploads=("$IPA" "$CHECKSUM")
: > obsolete-assets.txt
release_asset_plan() {
  python3 - "$1" "$IPA" "$CHECKSUM" <<'PYUPLOAD'
import hashlib, json, sys
from pathlib import Path

mode = sys.argv[1]
files = [Path(value) for value in sys.argv[2:]]
release = json.load(open("release-state.json"))
ready = {
    asset["name"]: asset for asset in release["assets"]
    if asset.get("state") == "uploaded" and asset.get("size", 0) > 0
}
if mode == "plan" and release["isDraft"]:
    print(*(str(path) for path in files), sep="\n")
    raise SystemExit(0)

for path in files:
    uploaded = ready.get(path.name)
    if uploaded is None:
        if mode == "verify":
            raise SystemExit(f"GitHub did not confirm the upload of {path.name}")
        print(path)
        continue
    with path.open("rb") as stream:
        expected = "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()
    if uploaded.get("digest") != expected:
        raise SystemExit(f"Published asset {path.name} does not match this build")
PYUPLOAD
}

check_draft_target() {
  # GitHub only uses targetCommitish when it still has to create the tag.
  python3 - "$GITHUB_SHA" <<'PYTARGET'
import json, sys

release = json.load(open("release-state.json"))
if not release["isDraft"] or release["targetCommitish"] != sys.argv[1]:
    raise SystemExit("Release has no matching tag or draft commit")
PYTARGET
}

if gh release view "$tag" --json isDraft,targetCommitish,assets > release-state.json; then
  if [[ "$tag_exists" == false ]]; then check_draft_target; fi
  release_asset_plan plan > release-upload-plan.txt
  uploads=()
  while IFS= read -r asset; do
    [[ -z "$asset" ]] || uploads+=("$asset")
  done < release-upload-plan.txt
  # Remove only obsolete Horizon IPA/checksum names for this exact build after
  # uploading its corrected pair. Unrelated attachments are never touched.
  python3 - "$ipa_name" "$checksum_name" "$BUILD" <<'PYASSETS' > obsolete-assets.txt
import json, re, sys
keep = set(sys.argv[1:3])
pattern = re.compile(r"Horizon-iOS-[0-9]+(?:\.[0-9]+){0,2}-build" + re.escape(sys.argv[3]) + r"-unsigned\.ipa(?:\.sha256)?")
for asset in json.load(open("release-state.json"))["assets"]:
    name = asset["name"]
    if name not in keep and pattern.fullmatch(name):
        print(name)
PYASSETS
else
  # A failed release view can also mean the API is unreachable or the
  # response could not be read. Confirm absence, including pending draft tags.
  if ! missing=$(gh api graphql \
    -f query='query($owner: String!, $name: String!, $tag: String!) {
      repository(owner: $owner, name: $name) { release(tagName: $tag) { id } }
    }' \
    -f owner="${GH_REPO%/*}" -f name="${GH_REPO#*/}" -f tag="$tag" \
    --jq '((.errors // []) | length == 0) and (.data.repository | type == "object" and has("release") and .release == null)'); then
    missing=false
  fi
  if [[ "$missing" != true ]]; then
    echo '::error::Could not confirm the release is absent; refusing to create a draft.'
    exit 1
  fi
  gh release create "$tag" --target "$GITHUB_SHA" --draft \
    --title "Horizon iOS $VERSION — build $BUILD" --notes-file release-notes.md
fi
if ((${#uploads[@]})); then
  gh release upload "$tag" "${uploads[@]}" --clobber
  # Verify the server-side bytes before retiring older downloads or publishing.
  gh release view "$tag" --json isDraft,targetCommitish,assets > release-state.json
  release_asset_plan verify
fi
check_release_tag
if [[ "$had_tag" == true && "$tag_exists" == false ]]; then
  echo '::error::Release tag disappeared during publication; stopping.'
  exit 1
fi
# The draft may have changed while the artifacts were being uploaded.
if [[ "$tag_exists" == false ]]; then check_draft_target; fi
while IFS= read -r obsolete; do
  [[ -z "$obsolete" ]] || gh release delete-asset "$tag" "$obsolete" --yes
done < obsolete-assets.txt
gh release edit "$tag" --title "Horizon iOS $VERSION — build $BUILD" \
  --notes-file release-notes.md --draft=false --prerelease=false --latest="$latest"
