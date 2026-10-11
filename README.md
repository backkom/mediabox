# mediabox

`mediabox` 是一个 Rust CLI 工具，用于把文件按日期归档，并在每个归档子目录内按内容去重。

## 构建

```bash
cargo build --release
```

生成的程序位于 `target/release/mediabox`。

在 macOS 上为你的 `x86_64` Linux NAS 构建：

```bash
./build-linux.sh
```

脚本需要 Docker，只生成本地文件：

```text
dist/mediabox-linux-x86_64
```

生成后可手动将该文件复制到 NAS。ARM64 NAS 可使用：

```bash
MEDIABOX_TARGET=aarch64-unknown-linux-musl \
DOCKER_PLATFORM=linux/arm64 \
./build-linux.sh
```

如果不想启动 Docker daemon，可以使用 Zig 作为 Linux linker：

```bash
brew install zig
./build-linux-zig.sh
```

该脚本同样生成 `dist/mediabox-linux-x86_64`，但需要本机安装 `zig` 和 `rustup`。

## move：归档文件

递归扫描源目录，将文件移动到 `destpath/year/month`：

```bash
mediabox move srcpath destpath
```

日期按以下顺序获取：

1. 文件所在目录或祖先目录名称，例如 `2025-07-13`。
2. 文件名，例如 `IMG_20200517_193506.mp4`。
3. 文件创建时间；创建时间不可用时回退到 `mtime`。

无法识别日期的文件默认进入 `destpath/unknown`，也可以指定其它相对目录：

```bash
mediabox move srcpath destpath --unknown-date needs-review
```

预览计划而不修改文件：

```bash
mediabox move srcpath destpath --dry-run
```

move 对普通文件不计算 hash，也不更新 `database.bin`。如果目标目录已有同名文件，会计算源文件和目标文件的 BLAKE3：相同则跳过，不同则生成 `name (1).ext` 这样的新名称，避免覆盖。`unique` 负责归档目录的完整去重和 `database.bin` 更新。

## unique：归档目录去重

按每个 `year/month` 子目录（以及 `unknown` 等未知日期目录）单独计算文件 BLAKE3 hash，只保留最老的一份：

```bash
mediabox unique destpath
```

重复文件会移动到 `destpath/.trash/`，不会直接删除。先预览：

```bash
mediabox unique destpath --dry-run
```

程序会跳过符号链接、`@` 开头的目录（例如 `@eaDir`）、`database.bin` 和回收站内容。跨文件系统移动会使用安全的复制后删除流程，并保留基础权限和 `mtime`；ACL、xattr、SELinux label 等扩展属性不主动复制。

完整设计见 [DESIGN.md](DESIGN.md)。

## 开发检查

```bash
cargo fmt --check
cargo test
```
