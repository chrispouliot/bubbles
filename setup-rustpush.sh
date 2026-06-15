#!/usr/bin/env bash
# Sets up the local rustpush clone that the `rustpush` feature builds against.
# Run from the project root (where this script lives):  ./setup-rustpush.sh
#
# Why a local clone instead of a git dependency:
#   rustpush .gitignores certs/fairplay/* (Apple FairPlay keys, not
#   redistributable) but activation.rs include_bytes!()s them at COMPILE time.
#   Upstream CI seeds fake certs by copying certs/legacy-fairplay/fairplay.{crt,pem}
#   onto each expected name. A ~/.cargo/git checkout can't hold those seeded
#   files, so the build must use a path dep pointing here.
set -euo pipefail

PIN=a7fab473e7a33325a760635285db2860de8e1cb0
DEST=third_party/rustpush   # must match the `path =` in Cargo.toml

# git@-style submodule URLs -> https, so --recursive works without SSH keys
git config --global url."https://github.com/".insteadOf "git@github.com:" || true

if [ ! -d "$DEST/.git" ]; then
  echo ">> cloning rustpush into $DEST"
  git clone https://github.com/OpenBubbles/rustpush.git "$DEST"
fi

echo ">> pinning $DEST to $PIN and syncing submodules"
git -C "$DEST" fetch --all --tags
git -C "$DEST" checkout "$PIN"
git -C "$DEST" submodule update --init --recursive

echo ">> seeding FairPlay certs from legacy-fairplay"
CERTS="$DEST/certs"
SRC="$CERTS/legacy-fairplay"
if [ ! -f "$SRC/fairplay.crt" ] || [ ! -f "$SRC/fairplay.pem" ]; then
  echo "!! $SRC/fairplay.{crt,pem} missing -- wrong commit or layout changed" >&2
  exit 1
fi
mkdir -p "$CERTS/fairplay"
for name in \
  4056631661436364584235346952193 4056631661436364584235346952194 \
  4056631661436364584235346952195 4056631661436364584235346952196 \
  4056631661436364584235346952197 4056631661436364584235346952198 \
  4056631661436364584235346952199 4056631661436364584235346952200 \
  4056631661436364584235346952201 4056631661436364584235346952208 ; do
  cp "$SRC/fairplay.crt" "$CERTS/fairplay/$name.crt"
  cp "$SRC/fairplay.pem" "$CERTS/fairplay/$name.pem"
done

echo ">> seeded $(ls "$CERTS/fairplay" | wc -l) files into $CERTS/fairplay"
echo ">> done. now from the project root:"
echo "     export CARGO_NET_GIT_FETCH_WITH_CLI=true"
echo "     cargo build --features rustpush"
