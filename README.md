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

日期按以下顺序获取，**任一来源命中即立刻返回**：

1. 文件所在目录或祖先目录名称，例如 `2025-07-13`。
2. 文件名，例如 `IMG_20200517_193506.mp4`。
3. 照片的 EXIF 拍摄时间，依次尝试 `DateTimeOriginal`、`CreateDate`、`ModifyDate`。
4. 视频/音频容器内的创建时间，例如 MP4/MOV 的 `mvhd.creation_time`；iPhone 视频优先使用带时区的 `com.apple.quicktime.creationdate`。
5. 文件创建时间；创建时间不可用时回退到 `mtime`。

目录名和文件名排在媒体元数据之前，是因为解析名称不需要打开文件；只有当名称里没有日期时，才会读取文件内容解析拍摄时间。拍摄时间比文件系统时间更接近真实拍摄日期，因此排在文件系统时间之前。

支持的图片格式为 JPEG、PNG、WebP、TIFF、HEIC/HEIF、AVIF、CR3、RAF；视频/音频支持 MP4、MOV、3GP、MKV、WebM 等。

具体行为约定：

- **命中即返回**：四类来源按上述顺序短路，不会合并多个来源的信息。
- **静默回退**：媒体解析失败（格式不支持、文件损坏、不是媒体文件）不报错，只是安静地落到下一步。日志里的 `created`/`mtime` 只反映最终采用的来源，看不出中间是否尝试过 EXIF。
- **无效值不中断**：某个时间戳非法只会让**当前来源**失效，回退链继续向下；同一来源内部的多个候选标签也会继续尝试下一个。
- **时区不换算**：一律按媒体内记录的本地挂钟时间取日期，归档结果不依赖运行机器的时区设置。
- **合理性校验**：拍摄时间必须落在 1970 年至当前年份加一年之间，否则丢弃并继续回退。这能过滤掉视频容器中常见的 `creation_time=0`（解出 1904-01-01）以及相机时钟错误导致的未来时间。

> 由于名称中的日期优先于媒体拍摄时间，若文件名或目录名里的日期与照片真实拍摄时间不一致（例如文件名是导出日期），归档会采用名称中的日期。需要以拍摄时间为准时，请先重命名文件或清理目录名。

无法识别日期的文件默认进入 `destpath/unknown`，也可以指定其它相对目录：

```bash
mediabox move srcpath destpath --unknown-date needs-review
```

预览计划而不修改文件：

```bash
mediabox move srcpath destpath --dry-run
```

move 对普通文件不计算 hash，也不更新 `database.bin`。如果目标目录已有同名文件，会计算源文件和目标文件的 BLAKE3：相同则跳过，不同则生成 `name (1).ext` 这样的新名称，避免覆盖。`unique` 负责归档目录的完整去重和 `database.bin` 更新。

同名冲突默认使用完整 BLAKE3；需要更快的近似比较时，可使用 `--compare-mode fast`。快速模式先比较文件大小，再比较文件头部和尾部各最多 64 KiB 的采样内容，采样相同即视为相同，存在极低概率的误判。

```bash
mediabox move srcpath destpath --compare-mode fast
```

## unique：归档目录去重

按每个 `year/month` 子目录（以及 `unknown` 等未知日期目录）单独计算文件 BLAKE3 hash，只保留文件名最短的一份（长度相同按文件名排序）：

```bash
mediabox unique destpath
```

重复文件会移动到 `destpath/.trash/`，不会直接删除。先预览：

```bash
mediabox unique destpath --dry-run
```

`unique` 也支持相同的 `--compare-mode blake3|fast` 参数；`database.bin` 仍始终保存完整 BLAKE3。

使用 `--image-similarity` 可让 `unique` 将 `IMG_8829.JPG` 和 `IMG_8829(1).JPG` 归为候选组，再比较原始分辨率的像素内容，保留文件名最短的文件：

```bash
mediabox unique destpath --image-similarity
```

图片相似比较只在同一文件名组内进行，例如 `IMG_8829.JPG` 和 `IMG_8829(1).JPG`，不会把所有图片互相比较。该模式支持 JPEG、PNG、WebP，按原始分辨率逐像素比较。HEIC/HEIF 不进行像素解码，继续使用普通 BLAKE3 逻辑。

`--image-similarity` 和 `--compare-mode` 可以同时使用：前者控制图片候选组的像素比较，后者控制普通文件及未进入图片候选组的文件使用完整 BLAKE3 或快速采样比较。图片相似模式也支持 dry-run：

```bash
mediabox unique destpath --image-similarity --dry-run
```

dry-run 会执行扫描、图片解码和比较，但不会移动文件、创建回收站或更新 `database.bin`。

程序会跳过符号链接、`@` 开头的目录（例如 `@eaDir`）、`database.bin` 和回收站内容。跨文件系统移动会使用安全的复制后删除流程，并保留基础权限和 `mtime`；ACL、xattr、SELinux label 等扩展属性不主动复制。

`move` 的每行输出会标注日期来源：`exif`、`video-created`、`folder`、`filename`、`created`、`mtime` 或 `unknown`。

```text
[moved:exif] src/2019-01-02/IMG_20180304.jpg -> dest/2020/05/IMG_20180304.jpg
[moved:video-created] src/VID_20180304.mp4 -> dest/2026/08/VID_20180304.mp4
```

完整设计见 [DESIGN.md](DESIGN.md)。

## 开发检查

```bash
cargo fmt --check
cargo test
```
