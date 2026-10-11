#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET="${MEDIABOX_TARGET:-x86_64-unknown-linux-musl}"

case "$TARGET" in
  x86_64-unknown-linux-musl)
    ZIG_TARGET="x86_64-linux-musl"
    OUTPUT_NAME="mediabox-linux-x86_64"
    LINKER_NAME="zig-cc-x86_64-linux-musl"
    ;;
  aarch64-unknown-linux-musl)
    ZIG_TARGET="aarch64-linux-musl"
    OUTPUT_NAME="mediabox-linux-aarch64"
    LINKER_NAME="zig-cc-aarch64-linux-musl"
    ;;
  *)
    echo "不支持的 MEDIABOX_TARGET: $TARGET" >&2
    echo "支持: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl" >&2
    exit 2
    ;;
esac

if ! command -v zig >/dev/null 2>&1; then
  echo "未找到 zig，请先安装 Zig。" >&2
  exit 2
fi
if ! command -v rustup >/dev/null 2>&1; then
  echo "未找到 rustup，请先安装 Rust 工具链。" >&2
  exit 2
fi

rustup target add "$TARGET"
mkdir -p "$ROOT_DIR/.build-tools" "$ROOT_DIR/dist"
LINKER="$ROOT_DIR/.build-tools/$LINKER_NAME"
cat > "$LINKER" <<EOF_LINKER
#!/usr/bin/env bash
set -euo pipefail
args=()
for arg in "\$@"; do
  case "\$arg" in
    */rcrt1.o|*/crt1.o|*/crti.o|*/crtbegin*.o|*/crtend*.o|*/crtn.o) ;;
    *) args+=("\$arg") ;;
  esac
done
exec zig cc -target $ZIG_TARGET "\${args[@]}"
EOF_LINKER
chmod +x "$LINKER"

TARGET_ENV="$(printf '%s' "$TARGET" | tr '[:lower:]-' '[:upper:]_')"
env "CARGO_TARGET_${TARGET_ENV}_LINKER=$LINKER" \
  cargo build --release --target "$TARGET"

OUTPUT="$ROOT_DIR/dist/$OUTPUT_NAME"
cp "$ROOT_DIR/target/$TARGET/release/mediabox" "$OUTPUT"
chmod +x "$OUTPUT"
echo "构建完成: $OUTPUT"
if command -v file >/dev/null 2>&1; then
  file "$OUTPUT"
fi
