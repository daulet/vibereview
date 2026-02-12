#!/bin/bash
set -e

# Release script for vibereview
# Usage: ./scripts/release.sh <version>
# Example: ./scripts/release.sh 0.1.0

VERSION=${1:-$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')}

echo "Preparing release v${VERSION}..."

# Ensure we're on main branch and clean
if [[ $(git status --porcelain) ]]; then
    echo "Error: Working directory not clean. Commit or stash changes first."
    exit 1
fi

# Update version in Cargo.toml if needed
CURRENT_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [[ "$CURRENT_VERSION" != "$VERSION" ]]; then
    echo "Updating version from $CURRENT_VERSION to $VERSION..."
    sed -i '' "s/^version = \"$CURRENT_VERSION\"/version = \"$VERSION\"/" Cargo.toml
    cargo build --release  # Update Cargo.lock
    git add Cargo.toml Cargo.lock
    git commit -m "Bump version to $VERSION"
fi

# Build release
echo "Building release..."
cargo build --release

# Create git tag
echo "Creating tag v${VERSION}..."
git tag -a "v${VERSION}" -m "Release v${VERSION}"

# Push tag
echo "Pushing tag..."
git push origin main
git push origin "v${VERSION}"

echo ""
echo "Release v${VERSION} tagged and pushed!"
echo ""
echo "Next steps:"
echo "1. GitHub Actions will build and create the release automatically."
echo ""
echo "2. After GitHub release is created, update the formula:"
echo "   - Download the release tarballs:"
echo "     curl -sL https://github.com/daulet/vibereview/releases/download/v${VERSION}/vibereview-x86_64-apple-darwin.tar.gz -o /tmp/vibereview-x86_64.tar.gz"
echo "     curl -sL https://github.com/daulet/vibereview/releases/download/v${VERSION}/vibereview-aarch64-apple-darwin.tar.gz -o /tmp/vibereview-aarch64.tar.gz"
echo "   - Get SHA256:"
echo "     shasum -a 256 /tmp/vibereview-x86_64.tar.gz"
echo "     shasum -a 256 /tmp/vibereview-aarch64.tar.gz"
echo "   - Update homebrew-tap/Formula/vibereview.rb with the SHA256 values"
echo ""
