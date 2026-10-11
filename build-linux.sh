#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET="${MEDIABOX_TARGET:-x86_64-unknown-linux-musl}"
PLATFORM="${DOCKER_PLATFORM:-linux/amd64}"
IMAGE="${MEDIABOX_RUST_IMAGE:-rust:1.92-alpine}"

case "$TARGET" in
  x86_64-unknown-linux-musl)
    DEFAULT_OUTPUT_NAME="mediabox-linux-x86_64"
    ;;
  aarch64-unknown-linux-musl)
    DEFAULT_OUTPUT_NAME="mediabox-linux-aarch64"
    ;;
  *)
    echo "不支持的 MEDIABOX_TARGET: $TARGET" >&2
    echo "支持: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl" >&2
    exit 2
    ;;
esac

if ! command -v docker >/dev/null 2>&1; then
  echo "未找到 docker，请先安装 Docker Desktop 或设置等效构建环境。" >&2
  exit 2
fi
if ! docker info >/dev/null 2>&1; then
  echo "无法连接 Docker daemon，请先启动 Docker Desktop 或 Docker 服务。" >&2
  exit 2
fi

mkdir -p "$ROOT_DIR/dist"

echo "使用 $IMAGE 构建 $TARGET ..."
docker run --rm \
  --platform "$PLATFORM" \
  -e MEDIABOX_TARGET="$TARGET" \
  -v "$ROOT_DIR:/src" \
  -w /src \
  "$IMAGE" \
  sh -ec 'rustup target add "$MEDIABOX_TARGET" && cargo build --release --target "$MEDIABOX_TARGET"'

OUTPUT="$ROOT_DIR/dist/${MEDIABOX_OUTPUT_NAME:-$DEFAULT_OUTPUT_NAME}"
cp "$ROOT_DIR/target/$TARGET/release/mediabox" "$OUTPUT"
chmod +x "$OUTPUT"

echo "构建完成: $OUTPUT"
if command -v file >/dev/null 2>&1; then
  file "$OUTPUT"
fi
