#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
ZIP_PATH="${ROOT_DIR}/lambda.zip"

rm -rf "${DIST_DIR}" "${ZIP_PATH}"
mkdir -p "${DIST_DIR}"

pushd "${ROOT_DIR}" >/dev/null
npm install --omit=dev
cp -R src package.json node_modules "${DIST_DIR}/"
if [[ -f package-lock.json ]]; then
  cp package-lock.json "${DIST_DIR}/"
fi
popd >/dev/null

pushd "${DIST_DIR}" >/dev/null
zip -qr "${ZIP_PATH}" .
popd >/dev/null

echo "Created ${ZIP_PATH}"
