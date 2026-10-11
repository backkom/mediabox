use blake3::Hasher;
use chrono::{DateTime, Datelike, Local, NaiveDate};
use clap::{Args, Parser, Subcommand};
use filetime::{set_file_times, FileTime};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
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
}

#[derive(Args, Debug)]
struct UniqueArgs {
    destpath: PathBuf,
    #[arg(long)]
    dry_run: bool,
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
    Folder,
    Filename,
    Created,
    Modified,
    Unknown,
}

impl DateSource {
    fn as_str(self) -> &'static str {
        match self {
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
    let plans = build_move_plans(&src, &dest, &args.unknown_date)?;
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

fn build_move_plans(src: &Path, dest: &Path, unknown: &Path) -> AppResult<Vec<FilePlan>> {
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
            let source_hash = hash_file(&source)?;
            let target_hash = hash_file(&base_target)?;
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
            PlanAction::Move => plan_logs.push(format!(
                "[plan] {} -> {}",
                plan.source.display(),
                plan.target.display()
            )),
            PlanAction::SkipSame { existing, hash } => skip_logs.push(format!(
                "[skip-same] {} skipped; same as {} (BLAKE3 {})",
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
    let mut buffer = [0u8; 1024 * 1024];
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
        let mut by_hash: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for file in files {
            by_hash.entry(hash_file(&file)?).or_default().push(file);
        }
        for (hash, mut duplicates) in by_hash {
            if duplicates.len() < 2 {
                continue;
            }
            duplicates.sort_by_key(|path| age_key(path));
            let keep = duplicates[0].clone();
            println!("[duplicate] {} hash={}", group.display(), hash);
            println!("  keep: {}", keep.display());
            for delete in duplicates.into_iter().skip(1) {
                if !args.dry_run {
                    let current_hash = hash_file(&delete)?;
                    if current_hash != hash {
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
