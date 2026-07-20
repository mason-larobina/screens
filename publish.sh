#!/usr/bin/bash
set -xe
./format.sh
cargo +stable build
cargo +stable test
[[ -z "$(git status --porcelain)" ]] || exit 1

# Extract version from Cargo.toml before publishing
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ -n "$version" ]] || { echo "Failed to extract version from Cargo.toml" >&2; exit 1; }

git push
cargo publish "$@"

git tag "v$version"
git push origin "v$version"
