#!/usr/bin/env bash
# update-homebrew-formula.sh
#
# Downloads the release assets for a given tag, computes their SHA256 hashes,
# renders the Homebrew formula template, and pushes the result to the tap repo.
#
# Usage:
#   update-homebrew-formula.sh <version> <main-repo> <tap-repo>
#
# Arguments:
#   version    Git tag, e.g. "v1.2.3"
#   main-repo  GitHub repo that owns the release, e.g. "joaoh82/rustunnel"
#   tap-repo   GitHub repo for the Homebrew tap, e.g. "joaoh82/homebrew-rustunnel"
#
# Environment:
#   GH_TOKEN   GitHub token with write access to <tap-repo>
#
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "Usage: $0 <version> <main-repo> <tap-repo>"
  echo "Example: $0 v1.2.3 joaoh82/rustunnel joaoh82/homebrew-rustunnel"
  exit 1
fi

VERSION="$1"
MAIN_REPO="$2"
TAP_REPO="$3"

# Strip the leading 'v' for use inside the formula (version "1.2.3").
VERSION_WITHOUT_V="${VERSION#v}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="${SCRIPT_DIR}/../homebrew/rustunnel.rb.template"
TEMP_DIR="$(mktemp -d)"

cleanup() { rm -rf "$TEMP_DIR"; }
trap cleanup EXIT

echo "==> Updating Homebrew formula to ${VERSION}"

# ── Asset filenames (must match the packaging step in release.yml) ─────────────
declare -A ASSETS=(
  ["macos-arm64"]="rustunnel-${VERSION}-aarch64-apple-darwin.tar.gz"
  ["macos-x86_64"]="rustunnel-${VERSION}-x86_64-apple-darwin.tar.gz"
  ["linux-arm64"]="rustunnel-${VERSION}-aarch64-unknown-linux-gnu.tar.gz"
  ["linux-x86_64"]="rustunnel-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
)

declare -A CHECKSUMS

# ── Download each asset and compute its SHA256 ─────────────────────────────────
for key in "${!ASSETS[@]}"; do
  filename="${ASSETS[$key]}"
  echo "  Downloading ${filename}..."
  gh release download "${VERSION}" \
    --repo "${MAIN_REPO}" \
    --pattern "${filename}" \
    --dir "${TEMP_DIR}"

  if command -v sha256sum > /dev/null 2>&1; then
    checksum=$(sha256sum "${TEMP_DIR}/${filename}" | cut -d' ' -f1)
  elif command -v shasum > /dev/null 2>&1; then
    checksum=$(shasum -a 256 "${TEMP_DIR}/${filename}" | cut -d' ' -f1)
  else
    echo "Error: neither sha256sum nor shasum found" >&2
    exit 1
  fi

  CHECKSUMS[$key]="$checksum"
  echo "     SHA256 ${key}: ${checksum}"
done

# ── Render the formula from the template ───────────────────────────────────────
FORMULA_FILE="${TEMP_DIR}/rustunnel.rb"

sed \
  -e "s/{{VERSION}}/${VERSION_WITHOUT_V}/g" \
  -e "s/{{SHA256_MACOS_ARM64}}/${CHECKSUMS[macos-arm64]}/g" \
  -e "s/{{SHA256_MACOS_X86_64}}/${CHECKSUMS[macos-x86_64]}/g" \
  -e "s/{{SHA256_LINUX_ARM64}}/${CHECKSUMS[linux-arm64]}/g" \
  -e "s/{{SHA256_LINUX_X86_64}}/${CHECKSUMS[linux-x86_64]}/g" \
  "${TEMPLATE}" > "${FORMULA_FILE}"

echo "==> Generated formula:"
cat "${FORMULA_FILE}"

# ── Clone the tap repo, update the formula, and push ──────────────────────────
TAP_DIR="${TEMP_DIR}/tap"
echo "==> Cloning ${TAP_REPO}..."

# Configure git to authenticate using GH_TOKEN for all HTTPS operations.
git config --global url."https://x-access-token:${GH_TOKEN}@github.com/".insteadOf \
  "https://github.com/"

gh repo clone "${TAP_REPO}" "${TAP_DIR}" -- --depth 1

cp "${FORMULA_FILE}" "${TAP_DIR}/Formula/rustunnel.rb"

cd "${TAP_DIR}"
git config user.name  "github-actions[bot]"
git config user.email "github-actions[bot]@users.noreply.github.com"

if git diff --quiet; then
  echo "==> Formula is already up to date. Nothing to commit."
  exit 0
fi

git add Formula/rustunnel.rb
git commit -m "chore: update rustunnel formula to ${VERSION_WITHOUT_V}"
git push

echo "==> ✅ Homebrew formula updated to ${VERSION_WITHOUT_V}"
