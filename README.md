# mediabox

`mediabox` 是一个 Rust CLI 工具，用于把文件按日期归档，并在每个归档子目录内按内容去重。

## 构建

```bash
cargo build --release
```

生成的程序位于 `target/release/mediabox`。

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

目标目录已有同名且内容相同时会跳过；内容不同时会生成 `name (1).ext` 这样的名称。每个归档子目录会维护一个文本 `database.bin`，格式为 `hash<TAB>文件名`。

## unique：归档目录去重

按每个 `year/month` 子目录（以及 `unknown` 等未知日期目录）单独计算文件 BLAKE3 hash，只保留最老的一份：

```bash
mediabox unique destpath
```

重复文件会移动到 `destpath/.trash/`，不会直接删除。先预览：

```bash
mediabox unique destpath --dry-run
```

程序会跳过符号链接、`database.bin` 和回收站内容。跨文件系统移动会使用安全的复制后删除流程，并保留基础权限和 `mtime`；ACL、xattr、SELinux label 等扩展属性不主动复制。

完整设计见 [DESIGN.md](DESIGN.md)。

## 开发检查

```bash
cargo fmt --check
cargo test
```
