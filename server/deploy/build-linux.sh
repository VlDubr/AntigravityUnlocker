#!/usr/bin/env bash
# Cross-compile antigravity-server for a Linux VPS as a fully static musl
# binary using Docker (no host Rust/Linux toolchain required).
#
# Usage (from repo root, Docker daemon running):
#   bash server/deploy/build-linux.sh            # x86_64 static musl
#   TARGET=aarch64-unknown-linux-musl bash server/deploy/build-linux.sh
#
# Output: server/target/<target>/release/antigravity-server  (static ELF)
set -euo pipefail

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
IMAGE="${IMAGE:-rust:1.99-alpine}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

case "$TARGET" in
  x86_64-unknown-linux-musl)  ;;
  aarch64-unknown-linux-musl) ;;
  *) echo "unsupported TARGET=$TARGET (use x86_64/aarch64-unknown-linux-musl)" >&2; exit 1 ;;
esac

echo "Building $TARGET in $IMAGE ..."
docker run --rm \
  -v "$REPO_ROOT/server":/build \
  -w /build \
  -e CARGO_TARGET_DIR=/build/target \
  "$IMAGE" \
  sh -c "
    set -e
    apk add --no-cache musl-dev >/dev/null
    rustup target add $TARGET
    cargo build --release --target $TARGET
  "

BIN="server/target/$TARGET/release/antigravity-server"
echo "Built: $BIN"
file "$BIN" 2>/dev/null || true
ls -lh "$BIN"
echo "Copy to VPS:  scp $BIN user@vps:/opt/antigravity-server/antigravity-server"