#!/usr/bin/env bash
# Rewrite packaging/aur/PKGBUILD for a new release.
#
# Usage: update-pkgbuild.sh <version> <sha256-x86_64> <sha256-aarch64>
#   version is the release tag without the leading "v", e.g. 0.1.0-rc.2
set -euo pipefail

version="${1:?usage: update-pkgbuild.sh <version> <sha256-x86_64> <sha256-aarch64>}"
sha_x86="${2:?missing x86_64 sha256}"
sha_arm="${3:?missing aarch64 sha256}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkgbuild="${here}/PKGBUILD"

sed -i \
  -e "s|^_tag=.*|_tag=${version}|" \
  -e "s|^sha256sums_x86_64=.*|sha256sums_x86_64=('${sha_x86}')|" \
  -e "s|^sha256sums_aarch64=.*|sha256sums_aarch64=('${sha_arm}')|" \
  "${pkgbuild}"

echo "updated ${pkgbuild} for ${version}"
