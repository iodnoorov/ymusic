#!/bin/sh
# deb and rpm packages of the static release binary built by musl.sh:
#
#   packaging/musl.sh x86_64 build --locked --release
#   packaging/packages.sh x86_64 <output dir>
#
# Needs cargo-deb and cargo-generate-rpm (`cargo install cargo-deb cargo-generate-rpm`).
set -eu

arch=${1:?usage: $0 x86_64|aarch64 <output dir>}
out=${2:?usage: $0 x86_64|aarch64 <output dir>}
case $arch in
x86_64 | aarch64) ;;
*) echo "unsupported architecture: $arch" >&2 && exit 1 ;;
esac
target=$arch-unknown-linux-musl
cd "$(dirname "$0")/.."
test -x "target/$target/release/ymusic" || { echo "no target/$target/release/ymusic: build it with musl.sh" >&2 && exit 1; }
mkdir -p "$out"
cargo deb --no-build --no-strip --target "$target" --output "$out"
cargo generate-rpm --target "$target" --output "$out"
