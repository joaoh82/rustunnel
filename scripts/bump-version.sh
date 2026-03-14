#!/usr/bin/env bash
# bump-version.sh — update version in all crate Cargo.toml files
#
# Usage:
#   ./scripts/bump-version.sh <new-version>
#
# Example:
#   ./scripts/bump-version.sh 0.3.0

set -euo pipefail

# ── validate argument ──────────────────────────────────────────────────────────

if [[ $# -ne 1 ]]; then
    echo "Usage: $0 <new-version>"
    echo "Example: $0 0.3.0"
    exit 1
fi

NEW_VERSION="$1"

# Validate semver format (major.minor.patch, each part numeric)
if ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: version must be in semver format (e.g. 1.2.3), got: $NEW_VERSION"
    exit 1
fi

# ── locate repo root ───────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CRATES=(
    "crates/rustunnel-client/Cargo.toml"
    "crates/rustunnel-server/Cargo.toml"
    "crates/rustunnel-mcp/Cargo.toml"
    "crates/rustunnel-protocol/Cargo.toml"
)

# ── show current versions ──────────────────────────────────────────────────────

echo "Bumping version to $NEW_VERSION"
echo ""

OLD_VERSION=""
for CRATE in "${CRATES[@]}"; do
    FILE="$REPO_ROOT/$CRATE"
    if [[ ! -f "$FILE" ]]; then
        echo "Error: $FILE not found"
        exit 1
    fi
    CURRENT=$(grep '^version' "$FILE" | head -1 | sed 's/version = "\(.*\)"/\1/')
    if [[ -z "$OLD_VERSION" ]]; then
        OLD_VERSION="$CURRENT"
    fi
    echo "  $CRATE  ($CURRENT → $NEW_VERSION)"
done

echo ""

# ── confirm ────────────────────────────────────────────────────────────────────

read -r -p "Continue? [y/N] " CONFIRM
if [[ ! "$CONFIRM" =~ ^[Yy]$ ]]; then
    echo "Aborted."
    exit 0
fi

echo ""

# ── update files ───────────────────────────────────────────────────────────────

for CRATE in "${CRATES[@]}"; do
    FILE="$REPO_ROOT/$CRATE"
    # Replace only the first occurrence of ^version = "..." in each file
    # (avoids touching [dependencies] version fields)
    # awk is used for cross-platform compatibility (BSD sed on macOS lacks 0, addr)
    awk -v ver="$NEW_VERSION" '
        /^version = / && done == 0 { sub(/"[^"]*"/, "\"" ver "\""); done=1 }
        { print }
    ' "$FILE" > "$FILE.tmp" && mv "$FILE.tmp" "$FILE"
    echo "  Updated $CRATE"
done

echo ""

# ── rebuild workspace ──────────────────────────────────────────────────────────

echo "Running cargo build --workspace …"
echo ""
cd "$REPO_ROOT"
cargo build --workspace

echo ""
echo "Done. All crates are now at version $NEW_VERSION."
echo ""
echo "Next steps:"
echo "  git add -A"
echo "  git commit -m \"chore: bump version to $NEW_VERSION\""
echo "  git tag v$NEW_VERSION"
echo "  git push && git push --tags"