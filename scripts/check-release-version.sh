#!/usr/bin/env bash
# Fail unless a release tag matches the version in every file that declares one, because the
# installers take their version from these files, not from the tag. Run it before tagging:
#
#   scripts/check-release-version.sh v0.2.0
set -euo pipefail

tag="${1:?usage: check-release-version.sh vX.Y.Z}"
want="${tag#v}"
cd "$(dirname "$0")/.."

status=0
check() {
  if [ "$2" != "$want" ]; then
    echo "$1 says '$2', but the tag is $tag" >&2
    status=1
  fi
}

check Cargo.toml "$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
check src-tauri/tauri.conf.json "$(node -p "require('./src-tauri/tauri.conf.json').version")"
check ui/package.json "$(node -p "require('./ui/package.json').version")"

if [ "$status" -eq 0 ]; then
  echo "All versions match $tag"
fi
exit "$status"
