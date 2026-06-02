#!/bin/bash

set -e

VERSION=$(grep -m 1 "^version" Cargo.toml | sed -E 's/version\s*=\s*"([^"]*)"/\1/')
TAG="v$VERSION"

if [ -z "$VERSION" ]; then
    echo "Error: Could not determine version from Cargo.toml"
    exit 1
fi

echo "Found version: $VERSION"
echo "Using tag: $TAG"

CURRENT_BRANCH=$(git branch --show-current)
if [ "$CURRENT_BRANCH" != "main" ]; then
    echo "Not on main branch. Current branch: $CURRENT_BRANCH"
    exit 0
fi

echo "Currently on main branch. Proceeding..."

# Check if tag already exists remotely
if git ls-remote --tags origin | grep -q "refs/tags/$TAG\$"; then
    echo "Tag $TAG already exists in the remote repository."
    exit 0
fi

echo "Tag $TAG doesn't exist in the remote repository. Creating and pushing..."

git tag "$TAG"
git push origin "$TAG"

echo "Tag $TAG created and pushed successfully!"
