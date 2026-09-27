#!/usr/bin/env bash
# build, test, upload and install. Needs deploy/host.env and an SSH alias
# for the server (default personal-server, override with GH_IMG_SSH).
set -euo pipefail
cd "$(dirname "$0")/.."
host="${GH_IMG_SSH:-personal-server}"
target=x86_64-unknown-linux-musl

[ -r deploy/host.env ] || { echo "missing deploy/host.env; copy deploy/host.env.example" >&2; exit 1; }

cargo test --release
scripts/check-fixtures.sh
cargo zigbuild --release --target "$target"
file "target/$target/release/gh-img-server" | grep -q 'static' || { echo "binary is not static" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/deploy"
cp "target/$target/release/gh-img-server" "$stage/"
cp deploy/install.sh deploy/host.env deploy/*.in deploy/gh-img-sweep.* "$stage/deploy/"

rsync -a --delete "$stage/" "$host:gh-img-src/"
ssh "$host" 'sudo bash gh-img-src/deploy/install.sh'
