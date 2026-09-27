#!/usr/bin/env bash
# Add .rpm packages to the yum/dnf repository that GitHub Pages serves from
# rodent-software/packrat-repo.
#
# Usage: publish-rpm.sh <repo-root> <rpms-dir>
#
# Environment:
#   GPG_KEY_ID  signing key id (required); the private key must be imported
set -euo pipefail

repo="${1:?usage: publish-rpm.sh <repo-root> <rpms-dir>}"
rpms="${2:?usage: publish-rpm.sh <repo-root> <rpms-dir>}"
sign="${GPG_KEY_ID:?GPG_KEY_ID must be set}"

base="${repo}/rpm"
mkdir -p "${base}"
cp "${rpms}"/*.rpm "${base}/"

createrepo_c --database --update "${base}"
gpg --detach-sign --armor --local-user "${sign}" "${base}/repodata/repomd.xml"

# Publish the signing key next to the repository for `gpgkey=`.
gpg --armor --export "${sign}" > "${repo}/packrat.gpg"
echo "rpm repository updated"
