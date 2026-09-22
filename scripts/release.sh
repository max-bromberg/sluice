#!/usr/bin/env bash
# Cut a release: set the version, commit, tag, and push the tag. The Release
# workflow then builds, tests and publishes it.
#
#   scripts/release.sh 0.2.0
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

version=${1:?usage: scripts/release.sh X.Y.Z}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $version" >&2; exit 1; }
tag="v$version"

[[ $(git branch --show-current) == main ]] || { echo "release from main" >&2; exit 1; }
[[ -z $(git status --porcelain) ]] || { echo "the working tree is not clean" >&2; exit 1; }
git fetch --quiet origin main
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] || { echo "main is not in sync with origin" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    echo "$tag already exists" >&2
    exit 1
fi

sed -i "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
export PATH="$HOME/.cargo/bin:$PATH"
cargo update --workspace --quiet
cargo test --all-features --quiet

if ! git diff --quiet; then
    git commit -q -am "Release $tag"
fi
git tag -a "$tag" -m "sluice $version"
echo "About to push main and $tag; the Release workflow publishes it."
read -r -p "Push? [y/N] " answer
[[ $answer == [yY]* ]] || { echo "not pushed; undo with: git tag -d $tag && git reset --hard origin/main"; exit 1; }
git push origin main "$tag"
