use blake3::Hasher;
use chrono::{DateTime, Datelike, Local, NaiveDate};
use clap::{Args, Parser, Subcommand, ValueEnum};
use filetime::{set_file_times, FileTime};
use image::GenericImageView;
use nom_exif::{Exif, ExifTag, MediaKind, MediaParser, MediaSource, TrackInfoTag};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

const DEFAULT_UNKNOWN_DIR: &str = "unknown";
const TRASH_DIR: &str = ".trash";

#[derive(Parser, Debug)]
#[command(name = "mediabox", version, about = "按日期归档文件并按内容去重")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Move(MoveArgs),
    Unique(UniqueArgs),
}

#[derive(Args, Debug)]
struct MoveArgs {
    srcpath: PathBuf,
    destpath: PathBuf,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = DEFAULT_UNKNOWN_DIR)]
    unknown_date: PathBuf,
    /// 同名冲突时的内容比较方式；fast 只比较大小和文件头尾采样
    #[arg(long, value_enum, default_value_t = CompareMode::Blake3)]
    compare_mode: CompareMode,
}

#[derive(Args, Debug)]
struct UniqueArgs {
    destpath: PathBuf,
    #[arg(long)]
    dry_run: bool,
    /// 去重时的内容比较方式；fast 只比较大小和文件头尾采样
    #[arg(long, value_enum, default_value_t = CompareMode::Blake3)]
    compare_mode: CompareMode,
    /// 对带编号后缀的图片文件名执行像素相似去重
    #[arg(long)]
    image_similarity: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CompareMode {
    #[default]
    Blake3,
    Fast,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Message(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

type AppResult<T> = Result<T, AppError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DateSource {
    Exif,
    VideoCreated,
    Folder,
    Filename,
    Created,
    Modified,
    Unknown,
}

impl DateSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exif => "exif",
            Self::VideoCreated => "video-created",
            Self::Folder => "folder",
            Self::Filename => "filename",
            Self::Created => "created",
            Self::Modified => "mtime",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug)]
struct ArchiveDate {
    year: i32,
    month: u32,
    source: DateSource,
}

impl ArchiveDate {
    fn dir_name(&self) -> PathBuf {
        PathBuf::from(format!("{:04}/{:02}", self.year, self.month))
    }
}

#[derive(Debug)]
struct FilePlan {
    source: PathBuf,
    target_dir: PathBuf,
    target: PathBuf,
    date_source: DateSource,
    action: PlanAction,
}

#[derive(Debug)]
enum PlanAction {
    Move,
    SkipSame { existing: PathBuf, hash: String },
}

#[derive(Default)]
struct Database {
    entries: BTreeMap<String, String>,
}

impl Database {
    fn set(&mut self, name: String, hash: String) {
        self.entries.insert(name, hash);
    }

    fn save(&self, path: &Path) -> AppResult<()> {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::Message("database.bin 没有父目录".into()))?;
        fs::create_dir_all(parent).map_err(|e| io_err("创建 database.bin 父目录", e))?;
        let tmp = path.with_extension("bin.tmp");
        let file = File::create(&tmp).map_err(|e| io_err("创建 database.bin 临时文件", e))?;
        let mut writer = BufWriter::new(file);
        for (name, hash) in &self.entries {
            writeln!(writer, "{}\t{}", hash, escape(name))
                .map_err(|e| io_err("写入 database.bin", e))?;
        }
        writer.flush().map_err(|e| io_err("刷新 database.bin", e))?;
        writer
            .into_inner()
            .map_err(|e| io_err("关闭 database.bin", e.into_error()))?
            .sync_all()
            .map_err(|e| io_err("同步 database.bin", e))?;
        fs::rename(&tmp, path).map_err(|e| io_err("替换 database.bin", e))?;
        Ok(())
    }
}

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock(dest: &Path, dry_run: bool) -> AppResult<Option<LockGuard>> {
    if dry_run {
        return Ok(None);
    }
    fs::create_dir_all(dest).map_err(|e| io_err("创建归档目录", e))?;
    let path = dest.join(".mediabox.lock");
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(_) => Ok(Some(LockGuard { path })),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(AppError::Message(format!(
            "无法取得锁，已存在 {}",
            path.display()
        ))),
        Err(e) => Err(io_err("创建 mediabox 锁", e)),
    }
}

fn io_err(context: impl Into<String>, source: io::Error) -> AppError {
    AppError::Io {
        context: context.into(),
        source,
    }
}

fn main() {
    let result = match Cli::parse().command {
        Command::Move(args) => run_move(args),
        Command::Unique(args) => run_unique(args),
    };
    if let Err(error) = result {
        eprintln!("错误: {error}");
        std::process::exit(1);
    }
}

fn run_move(args: MoveArgs) -> AppResult<()> {
    let src = fs::canonicalize(&args.srcpath).map_err(|e| io_err("读取源目录", e))?;
    let dest = if args.destpath.exists() {
        fs::canonicalize(&args.destpath).map_err(|e| io_err("读取归档目录", e))?
    } else if args.destpath.is_absolute() {
        args.destpath.clone()
    } else {
        std::env::current_dir()
            .map_err(|e| io_err("读取当前目录", e))?
            .join(&args.destpath)
    };
    if !src.is_dir() {
        return Err(AppError::Message(format!(
            "源路径不是目录: {}",
            src.display()
        )));
    }
    if dest.exists() && !dest.is_dir() {
        return Err(AppError::Message(format!(
            "归档路径不是目录: {}",
            dest.display()
        )));
    }
    if dest.starts_with(&src) {
        return Err(AppError::Message("destpath 不能位于 srcpath 内".into()));
    }
    validate_unknown_dir(&args.unknown_date)?;
    let _lock = acquire_lock(&dest, args.dry_run)?;
    let plans = build_move_plans(&src, &dest, &args.unknown_date, args.compare_mode)?;
    print_move_plans(&plans);
    if args.dry_run {
        return Ok(());
    }
    let mut counts = Counts::default();
    let mut moved_logs = Vec::new();
    let mut failed_logs = Vec::new();
    for plan in plans {
        match plan.action {
            PlanAction::SkipSame { .. } => {
                counts.skipped += 1;
            }
            PlanAction::Move => match execute_move(&plan) {
                Ok(log) => {
                    counts.moved += 1;
                    moved_logs.push(log);
                }
                Err(e) => {
                    counts.failed += 1;
                    failed_logs.push(format!("[failed] {}: {}", plan.source.display(), e));
                }
            },
        }
    }
    for log in moved_logs {
        println!("{log}");
    }
    for log in failed_logs {
        println!("{log}");
    }
    println!(
        "汇总: moved={}, skipped={}, failed={}",
        counts.moved, counts.skipped, counts.failed
    );
    if counts.failed > 0 {
        return Err(AppError::Message("部分文件处理失败".into()));
    }
    Ok(())
}

fn validate_unknown_dir(path: &Path) -> AppResult<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(AppError::Message(
            "--unknown-date 必须是 destpath 下的相对目录，不能包含绝对路径或 ..".into(),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct Counts {
    moved: usize,
    skipped: usize,
    failed: usize,
}

fn build_move_plans(
    src: &Path,
    dest: &Path,
    unknown: &Path,
    compare_mode: CompareMode,
) -> AppResult<Vec<FilePlan>> {
    let mut files = Vec::new();
    for entry_result in WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_visit_entry)
    {
        let entry = entry_result.map_err(|e| AppError::Message(format!("扫描源目录失败: {e}")))?;
        if entry.file_type().is_symlink() || !entry.file_type().is_file() {
            continue;
        }
        files.push(entry.into_path());
    }
    files.sort();
    let mut occupied = HashSet::new();
    let mut plans = Vec::new();
    for source in files {
        let (target_dir, date_source, _date_error) = match resolve_date(&source, src) {
            Ok(date) => (dest.join(date.dir_name()), date.source, None),
            Err(reason) => (dest.join(unknown), DateSource::Unknown, Some(reason)),
        };
        let base_target =
            target_dir.join(source.file_name().unwrap_or_else(|| OsStr::new("unnamed")));
        let target_is_regular_file = fs::symlink_metadata(&base_target)
            .map(|metadata| metadata.file_type().is_file())
            .unwrap_or(false);
        let (target, action) = if target_is_regular_file && !occupied.contains(&base_target) {
            let source_hash = compare_fingerprint(&source, compare_mode)?;
            let target_hash = compare_fingerprint(&base_target, compare_mode)?;
            if source_hash == target_hash {
                (
                    base_target.clone(),
                    PlanAction::SkipSame {
                        existing: base_target.clone(),
                        hash: source_hash,
                    },
                )
            } else {
                (
                    choose_target(&target_dir, &source, &occupied),
                    PlanAction::Move,
                )
            }
        } else if base_target.exists() || occupied.contains(&base_target) {
            (
                choose_target(&target_dir, &source, &occupied),
                PlanAction::Move,
            )
        } else {
            (base_target, PlanAction::Move)
        };
        if !matches!(action, PlanAction::SkipSame { .. }) {
            occupied.insert(target.clone());
        }
        plans.push(FilePlan {
            source,
            target_dir,
            target,
            date_source,
            action,
        });
    }
    Ok(plans)
}

fn choose_target(dir: &Path, source: &Path, occupied: &HashSet<PathBuf>) -> PathBuf {
    let target = dir.join(source.file_name().unwrap_or_else(|| OsStr::new("unnamed")));
    if !target.exists() && !occupied.contains(&target) && !is_reserved_target(&target) {
        return target;
    }
    let name = source
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("unnamed");
    let (stem, ext) = split_name(name);
    for n in 1..u64::MAX {
        let candidate_name = match ext {
            Some(ext) => format!(
                "{} ({}){}{}",
                stem,
                n,
                if ext.is_empty() { "" } else { "." },
                ext
            ),
            None => format!("{} ({})", stem, n),
        };
        let candidate = dir.join(candidate_name);
        if !candidate.exists() && !occupied.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn split_name(name: &str) -> (&str, Option<&str>) {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() && !ext.is_empty() {
            return (stem, Some(ext));
        }
    }
    (name, None)
}

fn execute_move(plan: &FilePlan) -> AppResult<String> {
    fs::create_dir_all(&plan.target_dir).map_err(|e| io_err("创建目标目录", e))?;
    let final_target = reserve_target(&plan.target)?;
    move_file(&plan.source, &final_target)?;
    Ok(format!(
        "[moved:{}] {} -> {}",
        plan.date_source.as_str(),
        plan.source.display(),
        final_target.display()
    ))
}

fn reserve_target(target: &Path) -> AppResult<PathBuf> {
    if !target.exists() && !is_reserved_target(target) {
        return Ok(target.to_path_buf());
    }
    let source_name = target
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("unnamed");
    let (stem, ext) = split_name(source_name);
    for n in 1..u64::MAX {
        let name = match ext {
            Some(ext) => format!(
                "{} ({}){}{}",
                stem,
                n,
                if ext.is_empty() { "" } else { "." },
                ext
            ),
            None => format!("{} ({})", stem, n),
        };
        let candidate = target.with_file_name(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(AppError::Message(format!(
        "无法为 {} 生成冲突文件名",
        target.display()
    )))
}

fn is_reserved_target(path: &Path) -> bool {
    matches!(
        path.file_name(),
        Some(name) if name == OsStr::new("database.bin") || name == OsStr::new(".mediabox.lock")
    )
}

fn move_file(source: &Path, target: &Path) -> AppResult<()> {
    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => copy_then_remove(source, target),
        Err(e) => Err(io_err(format!("移动 {}", source.display()), e)),
    }
}

fn copy_then_remove(source: &Path, target: &Path) -> AppResult<()> {
    let tmp = target.with_extension(format!("mediabox-{}.tmp", std::process::id()));
    let mut input = File::open(source).map_err(|e| io_err("打开源文件", e))?;
    let mut output = File::create(&tmp).map_err(|e| io_err("创建跨文件系统临时文件", e))?;
    io::copy(&mut input, &mut output).map_err(|e| io_err("复制文件", e))?;
    output.sync_all().map_err(|e| io_err("同步复制文件", e))?;
    fs::rename(&tmp, target).map_err(|e| io_err("提交复制文件", e))?;
    let metadata = fs::metadata(source).map_err(|e| io_err("读取源文件属性", e))?;
    let _ = fs::set_permissions(target, metadata.permissions());
    if let Ok(modified) = metadata.modified() {
        let mtime = FileTime::from_system_time(modified);
        let _ = set_file_times(target, mtime, mtime);
    }
    fs::remove_file(source).map_err(|e| io_err("删除已复制的源文件", e))?;
    Ok(())
}

fn print_move_plans(plans: &[FilePlan]) {
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    let mut skip_logs = Vec::new();
    let mut plan_logs = Vec::new();
    for plan in plans {
        let key = plan.target_dir.to_string_lossy().to_string();
        *groups.entry(key).or_default() += 1;
        match &plan.action {
            PlanAction::Move => {
                let source_name = plan.source.file_name();
                let target_name = plan.target.file_name();
                let label = if source_name != target_name {
                    "[plan_surffix]"
                } else {
                    "[plan]"
                };
                plan_logs.push(format!(
                    "{} {} -> {}",
                    label,
                    plan.source.display(),
                    plan.target.display()
                ));
            }
            PlanAction::SkipSame { existing, hash } => skip_logs.push(format!(
                "[skip-same] {} skipped; same as {} (fingerprint {})",
                plan.source.display(),
                existing.display(),
                hash
            )),
        }
    }
    for log in skip_logs.into_iter().chain(plan_logs) {
        println!("{log}");
    }
    println!("计划: {} 个文件，{} 个目标目录", plans.len(), groups.len());
    for (dir, count) in groups {
        println!("  {}: {} 个", dir, count);
    }
}

fn resolve_date(path: &Path, src_root: &Path) -> Result<ArchiveDate, String> {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if let Some(date) = parse_date_text(dir.file_name().and_then(OsStr::to_str).unwrap_or("")) {
            return Ok(ArchiveDate {
                year: date.year(),
                month: date.month(),
                source: DateSource::Folder,
            });
        }
        if dir == src_root {
            break;
        }
        parent = dir.parent();
    }
    if let Some(name) = path.file_name().and_then(OsStr::to_str) {
        if let Some(date) = parse_date_text(name) {
            return Ok(ArchiveDate {
                year: date.year(),
                month: date.month(),
                source: DateSource::Filename,
            });
        }
    }
    // 媒体内部的拍摄时间（EXIF DateTimeOriginal、视频 mvhd creation_time）比文件
    // 系统时间更接近真实拍摄日期，但需要读取文件内容，因此排在目录名和文件名之后：
    // 名称里已经有日期时就不必打开文件。
    if let Some(capture) = capture_date(path) {
        return Ok(ArchiveDate {
            year: capture.date.year(),
            month: capture.date.month(),
            source: capture.source,
        });
    }
    let metadata = fs::metadata(path).map_err(|e| format!("读取时间失败: {e}"))?;
    let created = metadata.created().ok();
    let modified = metadata.modified().ok();
    let time = created
        .or(modified)
        .ok_or_else(|| "无法取得创建时间或 mtime".to_string())?;
    let date = DateTime::<Local>::from(time).date_naive();
    Ok(ArchiveDate {
        year: date.year(),
        month: date.month(),
        source: if created.is_some() {
            DateSource::Created
        } else {
            DateSource::Modified
        },
    })
}

struct CaptureDate {
    date: NaiveDate,
    source: DateSource,
}

/// 读取媒体文件内部的拍摄时间：图片用 EXIF，视频/音频用容器创建时间。
/// 解析失败或文件不是可识别的媒体格式时返回 `None`，由调用方回退到其它来源。
fn capture_date(path: &Path) -> Option<CaptureDate> {
    let source = MediaSource::open(path).ok()?;
    let mut parser = MediaParser::new();
    let (date, source_kind) = match source.kind() {
        MediaKind::Image => {
            let exif: Exif = parser.parse_exif(source).ok()?.into();
            (image_capture_date(&exif)?, DateSource::Exif)
        }
        MediaKind::Track => {
            let track = parser.parse_track(source).ok()?;
            let value = track.get(TrackInfoTag::CreateDate)?.as_datetime()?;
            (value.into_naive().date(), DateSource::VideoCreated)
        }
    };
    // 容器或 EXIF 里常出现 0 值时间戳（例如 mvhd 的 creation_time=0 会解出
    // 1904-01-01）以及相机时钟错误导致的未来时间，这些不应参与归档。
    if !is_plausible_capture_date(date) {
        return None;
    }
    Some(CaptureDate {
        date,
        source: source_kind,
    })
}

/// 拍摄时间必须落在可信区间内，否则视为无效元数据。
fn is_plausible_capture_date(date: NaiveDate) -> bool {
    const EARLIEST_CAPTURE_YEAR: i32 = 1970;
    date.year() >= EARLIEST_CAPTURE_YEAR && date.year() <= Local::now().year() + 1
}

/// 取 EXIF 拍摄时间，优先级为 DateTimeOriginal > CreateDate > ModifyDate。
fn image_capture_date(exif: &Exif) -> Option<NaiveDate> {
    let tags = [
        ExifTag::DateTimeOriginal,
        ExifTag::CreateDate,
        ExifTag::ModifyDate,
    ];
    tags.into_iter().find_map(|tag| {
        let value = exif.get(tag)?.as_datetime()?;
        // 一律使用媒体内记录的本地挂钟时间，不换算时区，这样归档结果不依赖
        // 运行机器的时区设置。
        Some(value.into_naive().date())
    })
}

fn parse_date_text(text: &str) -> Option<NaiveDate> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if !text.is_char_boundary(i) {
            continue;
        }
        if i + 10 <= bytes.len() {
            if !text.is_char_boundary(i + 10) {
                continue;
            }
            let candidate = &text[i..i + 10];
            for fmt in ["%Y-%m-%d", "%Y_%m_%d", "%Y.%m.%d"] {
                if let Ok(date) = NaiveDate::parse_from_str(candidate, fmt) {
                    return Some(date);
                }
            }
        }
        if i + 8 <= bytes.len() {
            if !text.is_char_boundary(i + 8) {
                continue;
            }
            let candidate = &text[i..i + 8];
            if candidate.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(date) = NaiveDate::parse_from_str(candidate, "%Y%m%d") {
                    return Some(date);
                }
            }
        }
    }
    for i in 0..bytes.len() {
        if !text.is_char_boundary(i) {
            continue;
        }
        if i + 7 <= bytes.len() {
            if !text.is_char_boundary(i + 7) {
                continue;
            }
            let candidate = &text[i..i + 7];
            if candidate.as_bytes()[4] == b'-'
                || candidate.as_bytes()[4] == b'_'
                || candidate.as_bytes()[4] == b'.'
            {
                let normalized = candidate.replace(['_', '.'], "-");
                if let Ok(date) =
                    NaiveDate::parse_from_str(&format!("{}-01", normalized), "%Y-%m-%d")
                {
                    return Some(date);
                }
            }
        }
    }
    None
}

fn hash_file(path: &Path) -> AppResult<String> {
    let mut file =
        File::open(path).map_err(|e| io_err(format!("打开文件 {}", path.display()), e))?;
    let mut hasher = Hasher::new();
    // 放在堆上：1 MiB 的栈数组会溢出默认的 1 MiB 主线程栈。
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|e| io_err("读取文件 hash", e))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn compare_fingerprint(path: &Path, mode: CompareMode) -> AppResult<String> {
    match mode {
        CompareMode::Blake3 => hash_file(path),
        CompareMode::Fast => fast_fingerprint(path),
    }
}

const FAST_SAMPLE_BYTES: usize = 64 * 1024;

fn fast_fingerprint(path: &Path) -> AppResult<String> {
    let metadata = fs::metadata(path).map_err(|e| io_err("读取文件大小", e))?;
    let size = metadata.len();
    let sample_len = (size as usize).min(FAST_SAMPLE_BYTES);
    let mut file = File::open(path).map_err(|e| io_err("打开文件进行快速比较", e))?;
    let mut head = vec![0u8; sample_len];
    file.read_exact(&mut head)
        .map_err(|e| io_err("读取文件头部采样", e))?;
    let tail_offset = size.saturating_sub(FAST_SAMPLE_BYTES as u64);
    file.seek(SeekFrom::Start(tail_offset))
        .map_err(|e| io_err("定位文件尾部采样", e))?;
    let tail_len = (size - tail_offset) as usize;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail)
        .map_err(|e| io_err("读取文件尾部采样", e))?;
    let mut hasher = Hasher::new();
    hasher.update(&head);
    hasher.update(&tail);
    Ok(format!(
        "fast:size={size}:sample={}",
        hasher.finalize().to_hex()
    ))
}

fn image_duplicate_groups(
    files: &[PathBuf],
    compare_mode: CompareMode,
) -> AppResult<Vec<(String, Vec<PathBuf>)>> {
    let mut named: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut ordinary = Vec::new();
    for file in files {
        if let Some(key) = normalized_image_name(file) {
            named.entry(key).or_default().push(file.clone());
        } else {
            ordinary.push(file.clone());
        }
    }
    let mut result = Vec::new();
    let mut by_hash: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for file in ordinary {
        by_hash
            .entry(compare_fingerprint(&file, compare_mode)?)
            .or_default()
            .push(file);
    }
    result.extend(by_hash);
    for (name, group) in named {
        if group.len() < 2 {
            continue;
        }
        let mut components: Vec<Vec<PathBuf>> = Vec::new();
        for file in group {
            let mut matched = None;
            for (index, component) in components.iter().enumerate() {
                let similar = match images_similar(&file, &component[0]) {
                    Ok(value) => value,
                    Err(error) => {
                        eprintln!(
                            "[image-skip] {} 无法解码，跳过像素比较: {}",
                            file.display(),
                            error
                        );
                        false
                    }
                };
                if similar {
                    matched = Some(index);
                    break;
                }
            }
            if let Some(index) = matched {
                components[index].push(file);
            } else {
                components.push(vec![file]);
            }
        }
        for component in components {
            if component.len() > 1 {
                result.push((format!("image:{name}"), component));
            }
        }
    }
    Ok(result)
}

fn normalized_image_name(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    // image crate does not decode HEIC/HEIF in the portable static build;
    // those files fall back to the ordinary BLAKE3 path.
    if !matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let bytes = stem.as_bytes();
    let close = bytes.len().checked_sub(1)?;
    if bytes[close] != b')' {
        return Some(format!("{}.{}", stem.to_ascii_lowercase(), extension));
    }
    let open = stem[..close].rfind('(')?;
    let number = &stem[open + 1..close];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(format!("{}.{}", stem.to_ascii_lowercase(), extension));
    }
    let base = stem[..open].trim_end();
    if base.is_empty() {
        return None;
    }
    Some(format!("{}.{}", base.to_ascii_lowercase(), extension))
}

fn images_similar(left: &Path, right: &Path) -> AppResult<bool> {
    let left = image::open(left)
        .map_err(|e| AppError::Message(format!("读取图片 {}: {e}", left.display())))?;
    let right = image::open(right)
        .map_err(|e| AppError::Message(format!("读取图片 {}: {e}", right.display())))?;
    if left.dimensions() != right.dimensions() {
        return Ok(false);
    }
    let left = left.to_rgba8();
    let right = right.to_rgba8();
    let total: u64 = left
        .pixels()
        .zip(right.pixels())
        .map(|(a, b)| {
            u64::from(a[0].abs_diff(b[0]))
                + u64::from(a[1].abs_diff(b[1]))
                + u64::from(a[2].abs_diff(b[2]))
                + u64::from(a[3].abs_diff(b[3]))
        })
        .sum();
    let pixel_count = u64::from(left.width()) * u64::from(left.height());
    Ok(total as f64 / (pixel_count as f64 * 4.0) <= 6.0)
}

fn run_unique(args: UniqueArgs) -> AppResult<()> {
    let dest = fs::canonicalize(&args.destpath).map_err(|e| io_err("读取归档目录", e))?;
    if !dest.is_dir() {
        return Err(AppError::Message(format!(
            "归档路径不是目录: {}",
            dest.display()
        )));
    }
    let _lock = acquire_lock(&dest, args.dry_run)?;
    let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for entry_result in WalkDir::new(&dest)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_visit_entry)
    {
        let entry =
            entry_result.map_err(|e| AppError::Message(format!("扫描归档目录失败: {e}")))?;
        if entry.file_type().is_symlink() || !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .components()
            .any(|c| c.as_os_str() == OsStr::new(TRASH_DIR))
        {
            continue;
        }
        if path.file_name() == Some(OsStr::new("database.bin"))
            || path.file_name() == Some(OsStr::new(".mediabox.lock"))
        {
            continue;
        }
        let rel = path.strip_prefix(&dest).unwrap_or(path);
        let group = group_for_relative(rel).unwrap_or_else(|| PathBuf::from("unclassified"));
        groups.entry(group).or_default().push(path.to_path_buf());
    }
    let mut total_delete = 0usize;
    for (group, mut files) in groups {
        files.sort();
        let duplicate_groups = if args.image_similarity {
            image_duplicate_groups(&files, args.compare_mode)?
        } else {
            let mut by_hash: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
            for file in files {
                by_hash
                    .entry(compare_fingerprint(&file, args.compare_mode)?)
                    .or_default()
                    .push(file);
            }
            by_hash.into_iter().collect()
        };
        for (hash, mut duplicates) in duplicate_groups {
            if duplicates.len() < 2 {
                continue;
            }
            duplicates.sort_by_key(|path| age_key(path));
            let keep = duplicates[0].clone();
            println!("[duplicate] {} hash={}", group.display(), hash);
            println!("  keep: {}", keep.display());
            for delete in duplicates.into_iter().skip(1) {
                if !args.dry_run {
                    let unchanged = if hash.starts_with("image:") {
                        match images_similar(&delete, &keep) {
                            Ok(value) => value,
                            Err(error) => {
                                eprintln!(
                                    "[image-skip] {} 无法重新解码，跳过回收: {}",
                                    delete.display(),
                                    error
                                );
                                false
                            }
                        }
                    } else {
                        compare_fingerprint(&delete, args.compare_mode)? == hash
                    };
                    if !unchanged {
                        eprintln!(
                            "[changed-skip] {} 在扫描后内容已变化，跳过回收",
                            delete.display()
                        );
                        continue;
                    }
                    move_to_trash(&dest, &delete)?;
                    println!("  trash: {}", delete.display());
                } else {
                    println!("  delete: {}", delete.display());
                }
                total_delete += 1;
            }
        }
        if !args.dry_run {
            rebuild_database(&dest.join(&group))?;
        }
    }
    println!("去重汇总: 预计删除/已移入回收站 {} 个文件", total_delete);
    Ok(())
}

fn group_for_relative(rel: &Path) -> Option<PathBuf> {
    let mut components = rel.components();
    let first = components.next()?.as_os_str().to_owned();
    let second = components.next().map(|c| c.as_os_str().to_owned());
    let first_s = first.to_string_lossy();
    if first_s.len() == 4 && first_s.chars().all(|c| c.is_ascii_digit()) {
        if let Some(second) = second {
            return Some(PathBuf::from(first).join(second));
        }
    }
    Some(PathBuf::from(first))
}

fn age_key(path: &Path) -> (SystemTime, String) {
    let metadata = fs::metadata(path).ok();
    let time = metadata
        .as_ref()
        .and_then(|m| m.created().ok())
        .or_else(|| metadata.as_ref().and_then(|m| m.modified().ok()))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    (time, path.to_string_lossy().to_string())
}

fn move_to_trash(dest: &Path, source: &Path) -> AppResult<()> {
    let trash = dest.join(TRASH_DIR);
    fs::create_dir_all(&trash).map_err(|e| io_err("创建回收站目录", e))?;
    let base = source
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("unnamed");
    let mut target = trash.join(base);
    for n in 1..u64::MAX {
        if !target.exists() {
            break;
        }
        target = trash.join(format!("{} ({})", base, n));
    }
    fs::rename(source, &target).map_err(|e| io_err("移动重复文件到回收站", e))?;
    Ok(())
}

fn rebuild_database(dir: &Path) -> AppResult<()> {
    let path = dir.join("database.bin");
    let mut db = Database::default();
    if dir.is_dir() {
        for entry_result in WalkDir::new(dir)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(should_visit_entry)
        {
            let entry = entry_result
                .map_err(|e| AppError::Message(format!("重建 database.bin 失败: {e}")))?;
            if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if p.file_name() == Some(OsStr::new("database.bin")) {
                continue;
            }
            let name = p
                .strip_prefix(dir)
                .unwrap_or(p)
                .to_string_lossy()
                .to_string();
            db.set(name, hash_file(p)?);
        }
    }
    db.save(&path)
}

fn should_visit_entry(entry: &DirEntry) -> bool {
    !entry.file_type().is_dir() || !is_ignored_folder_name(entry.file_name())
}

fn is_ignored_folder_name(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('@')
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个带 EXIF DateTimeOriginal 的最小 JPEG。
    /// 注释段用于把文件撑过 MIME 探测使用的 128 字节缓冲。
    fn exif_jpeg(date_text: &str) -> Vec<u8> {
        const EXIF_IFD_OFFSET: u32 = 26;
        const STRING_OFFSET: u32 = 44;

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        // IFD0：一个 ExifOffset 条目，指向 Exif 子 IFD。
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x8769u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&EXIF_IFD_OFFSET.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        // Exif 子 IFD：一个 DateTimeOriginal 条目。
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x9003u16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&((date_text.len() + 1) as u32).to_le_bytes());
        tiff.extend_from_slice(&STRING_OFFSET.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(date_text.as_bytes());
        tiff.push(0);

        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);

        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        let padding = [b' '; 64];
        jpeg.extend_from_slice(&[0xFF, 0xFE]);
        jpeg.extend_from_slice(&((padding.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&padding);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    /// 构造一个不含 EXIF 的 JPEG（SOI + 注释段 + SOS + EOI）。
    fn plain_jpeg() -> Vec<u8> {
        let padding = [b' '; 64];
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xFE];
        jpeg.extend_from_slice(&((padding.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&padding);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]);
        jpeg
    }

    /// 构造一个带 moov/mvhd creation_time 的最小 MP4。
    fn mp4_with_creation_time(creation_time: u32) -> Vec<u8> {
        let mut mvhd_body = Vec::new();
        mvhd_body.extend_from_slice(&creation_time.to_be_bytes());
        mvhd_body.extend_from_slice(&creation_time.to_be_bytes());
        mvhd_body.extend_from_slice(&1000u32.to_be_bytes());
        mvhd_body.extend_from_slice(&1000u32.to_be_bytes());
        mvhd_body.extend_from_slice(&[0u8; 76]);
        mvhd_body.extend_from_slice(&1u32.to_be_bytes());

        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&((12 + mvhd_body.len()) as u32).to_be_bytes());
        mvhd.extend_from_slice(b"mvhd");
        mvhd.extend_from_slice(&[0, 0, 0, 0]);
        mvhd.extend_from_slice(&mvhd_body);

        let mut moov = Vec::new();
        moov.extend_from_slice(&((8 + mvhd.len()) as u32).to_be_bytes());
        moov.extend_from_slice(b"moov");
        moov.extend_from_slice(&mvhd);

        let mut ftyp_body = Vec::new();
        ftyp_body.extend_from_slice(b"isom");
        ftyp_body.extend_from_slice(&512u32.to_be_bytes());
        ftyp_body.extend_from_slice(b"isom");

        let mut file = Vec::new();
        file.extend_from_slice(&((8 + ftyp_body.len()) as u32).to_be_bytes());
        file.extend_from_slice(b"ftyp");
        file.extend_from_slice(&ftyp_body);
        file.extend_from_slice(&moov);
        file
    }

    /// 把 1904 纪元秒数换算为 mp4 使用的 creation_time。
    fn mp4_epoch_seconds(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> u32 {
        let target = NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, min, sec)
            .unwrap();
        let epoch = NaiveDate::from_ymd_opt(1904, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        (target - epoch).num_seconds() as u32
    }

    fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn reads_exif_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "photo.jpg", &exif_jpeg("2020:05:17 19:35:06"));
        let capture = capture_date(&path).expect("应解析出 EXIF 拍摄时间");
        assert_eq!(capture.date, NaiveDate::from_ymd_opt(2020, 5, 17).unwrap());
        assert_eq!(capture.source, DateSource::Exif);
    }

    #[test]
    fn reads_video_creation_time() {
        let dir = tempfile::tempdir().unwrap();
        let seconds = mp4_epoch_seconds(2021, 8, 9, 10, 11, 12);
        let path = write_fixture(dir.path(), "clip.mp4", &mp4_with_creation_time(seconds));
        let capture = capture_date(&path).expect("应解析出视频创建时间");
        assert_eq!(capture.date, NaiveDate::from_ymd_opt(2021, 8, 9).unwrap());
        assert_eq!(capture.source, DateSource::VideoCreated);
    }

    #[test]
    fn ignores_implausible_video_creation_time() {
        let dir = tempfile::tempdir().unwrap();
        // creation_time = 0 会解出 1904-01-01，属于占位值，必须忽略。
        let path = write_fixture(dir.path(), "zero.mp4", &mp4_with_creation_time(0));
        assert!(capture_date(&path).is_none());
    }

    #[test]
    fn jpeg_without_exif_has_no_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "plain.jpg", &plain_jpeg());
        assert!(capture_date(&path).is_none());
    }

    #[test]
    fn non_media_file_has_no_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "notes.txt", b"not a photo");
        assert!(capture_date(&path).is_none());
    }

    #[test]
    fn folder_name_outranks_capture_time() {
        // 名称里已有日期时不应再打开文件读媒体元数据。
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("2019-01-02");
        fs::create_dir_all(&folder).unwrap();
        let path = write_fixture(&folder, "plain-name.jpg", &exif_jpeg("2020:05:17 19:35:06"));
        let resolved = resolve_date(&path, dir.path()).unwrap();
        assert_eq!(resolved.year, 2019);
        assert_eq!(resolved.month, 1);
        assert_eq!(resolved.source, DateSource::Folder);
    }

    #[test]
    fn filename_outranks_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "IMG_20180304_050607.jpg",
            &exif_jpeg("2020:05:17 19:35:06"),
        );
        let resolved = resolve_date(&path, dir.path()).unwrap();
        assert_eq!(resolved.year, 2018);
        assert_eq!(resolved.month, 3);
        assert_eq!(resolved.source, DateSource::Filename);
    }

    #[test]
    fn video_filename_outranks_creation_time() {
        let dir = tempfile::tempdir().unwrap();
        let seconds = mp4_epoch_seconds(2021, 8, 9, 10, 11, 12);
        let path = write_fixture(
            dir.path(),
            "VID_20180304_050607.mp4",
            &mp4_with_creation_time(seconds),
        );
        let resolved = resolve_date(&path, dir.path()).unwrap();
        assert_eq!(resolved.year, 2018);
        assert_eq!(resolved.month, 3);
        assert_eq!(resolved.source, DateSource::Filename);
    }

    #[test]
    fn capture_time_used_when_names_have_no_date() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "DSC_0001.jpg", &exif_jpeg("2020:05:17 19:35:06"));
        let resolved = resolve_date(&path, dir.path()).unwrap();
        assert_eq!(resolved.year, 2020);
        assert_eq!(resolved.month, 5);
        assert_eq!(resolved.source, DateSource::Exif);
    }

    #[test]
    fn video_capture_time_used_when_names_have_no_date() {
        let dir = tempfile::tempdir().unwrap();
        let seconds = mp4_epoch_seconds(2021, 8, 9, 10, 11, 12);
        let path = write_fixture(dir.path(), "clip.mp4", &mp4_with_creation_time(seconds));
        let resolved = resolve_date(&path, dir.path()).unwrap();
        assert_eq!(resolved.year, 2021);
        assert_eq!(resolved.month, 8);
        assert_eq!(resolved.source, DateSource::VideoCreated);
    }

    #[test]
    fn capture_time_outranks_filesystem_time() {
        // 没有目录名和文件名日期时，媒体拍摄时间优先于文件系统时间。
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "DSC_0002.jpg", &exif_jpeg("2020:05:17 19:35:06"));
        let resolved = resolve_date(&path, dir.path()).unwrap();
        let filesystem_date = fs::metadata(&path)
            .unwrap()
            .created()
            .or_else(|_| fs::metadata(&path).unwrap().modified())
            .map(|time| DateTime::<Local>::from(time).date_naive())
            .unwrap();
        assert_eq!(resolved.source, DateSource::Exif);
        // 只有当文件系统日期与 EXIF 日期不同时，该断言才真正验证了优先级。
        if filesystem_date != NaiveDate::from_ymd_opt(2020, 5, 17).unwrap() {
            assert_eq!(resolved.year, 2020);
            assert_eq!(resolved.month, 5);
        }
    }

    #[test]
    fn folder_name_still_used_without_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("2019-01-02");
        fs::create_dir_all(&folder).unwrap();
        let path = write_fixture(&folder, "plain.jpg", &plain_jpeg());
        let resolved = resolve_date(&path, dir.path()).unwrap();
        assert_eq!(resolved.year, 2019);
        assert_eq!(resolved.month, 1);
        assert_eq!(resolved.source, DateSource::Folder);
    }

    #[test]
    fn parses_dates_in_names() {
        assert_eq!(
            parse_date_text("IMG_20200517_193506.mp4").unwrap().month(),
            5
        );
        assert_eq!(parse_date_text("2025-07-13").unwrap().year(), 2025);
        assert!(parse_date_text("2025-99-99").is_none());
        assert!(parse_date_text("照片😀.jpg").is_none());
    }

    #[test]
    fn escapes_database_names() {
        let value = "a\tb\nc";
        assert_eq!(escape(value), "a\\tb\\nc");
    }

    #[test]
    fn unknown_directory_must_stay_under_destination() {
        assert!(validate_unknown_dir(Path::new("../outside")).is_err());
        assert!(validate_unknown_dir(Path::new("needs-review")).is_ok());
    }

    #[test]
    fn ignores_at_prefixed_directories() {
        assert!(is_ignored_folder_name(OsStr::new("@eaDir")));
        assert!(is_ignored_folder_name(OsStr::new("@tmp")));
        assert!(!is_ignored_folder_name(OsStr::new("2025-07-13")));
    }
}
