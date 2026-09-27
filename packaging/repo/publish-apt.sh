#!/usr/bin/env bash
# Add .deb packages to the apt repository that GitHub Pages serves from
# rodent-software/packrat-repo.
#
# Usage: publish-apt.sh <repo-root> <debs-dir>
#
# Environment:
#   GPG_KEY_ID  signing key id (required); the private key must be imported
#   CODENAME    distribution codename (default: stable)
#   COMPONENT   component name (default: main)
set -euo pipefail

repo="${1:?usage: publish-apt.sh <repo-root> <debs-dir>}"
debs="${2:?usage: publish-apt.sh <repo-root> <debs-dir>}"
codename="${CODENAME:-stable}"
component="${COMPONENT:-main}"
sign="${GPG_KEY_ID:?GPG_KEY_ID must be set}"

base="${repo}/apt"
mkdir -p "${base}/conf"

cat > "${base}/conf/distributions" <<EOF
Origin: packrat
Label: packrat
Suite: ${codename}
Codename: ${codename}
Architectures: amd64 arm64
Components: ${component}
Description: packrat apt repository
SignWith: ${sign}
EOF

shopt -s nullglob
for deb in "${debs}"/*.deb; do
  echo "including ${deb}"
  reprepro --basedir "${base}" includedeb "${codename}" "${deb}"
done
shopt -u nullglob

# Publish the signing key next to the repository for `signed-by=`.
gpg --armor --export "${sign}" > "${repo}/packrat.gpg"
echo "apt repository updated"
