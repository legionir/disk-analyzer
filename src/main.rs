use ignore::{DirEntry, WalkBuilder, WalkState};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

const TOP_FILES_PER_DIRECTORY: usize = 10;
const MAX_ERROR_SAMPLES: usize = 50;
const PROGRESS_INTERVAL: u64 = 50_000;

#[derive(Debug)]
struct Config {
    root: PathBuf,
    excludes: Vec<PathBuf>,
    threads: usize,
    page_size: usize,
}

#[derive(Default)]
struct Counters {
    entries: AtomicU64,
    directories: AtomicU64,
    files: AtomicU64,
    skipped: AtomicU64,
    errors: AtomicU64,
    excluded: AtomicU64,
}

struct SharedScan {
    shards: Mutex<Vec<WorkerShard>>,
    counters: Counters,
}

#[derive(Default)]
struct WorkerShard {
    directories: HashMap<PathBuf, DirectoryStats>,
    errors: Vec<String>,
}

#[derive(Default)]
struct DirectoryStats {
    direct_bytes: u128,
    direct_files: u64,
    top_files: Vec<FileInfo>,
}

#[derive(Clone, Debug)]
struct FileInfo {
    path: PathBuf,
    size: u64,
}

struct WorkerState {
    shared: Arc<SharedScan>,
    root: PathBuf,
    directories: HashMap<PathBuf, DirectoryStats>,
    errors: Vec<String>,
}

#[derive(Debug)]
struct DirectoryNode {
    path: PathBuf,
    parent: Option<usize>,
    children: Vec<usize>,
    direct_bytes: u128,
    total_bytes: u128,
    direct_files: u64,
    total_files: u64,
    top_files: Vec<FileInfo>,
    depth: usize,
}

struct ScanResult {
    nodes: Vec<DirectoryNode>,
    root_index: usize,
    entries: u64,
    directories: u64,
    files: u64,
    skipped: u64,
    errors: u64,
    excluded: u64,
    error_samples: Vec<String>,
}

impl DirectoryStats {
    fn add_file(&mut self, path: &Path, size: u64) {
        self.direct_bytes += u128::from(size);
        self.direct_files += 1;

        let candidate = FileInfo {
            path: path.to_path_buf(),
            size,
        };

        insert_top_file(&mut self.top_files, candidate);
    }

    fn merge(&mut self, other: DirectoryStats) {
        self.direct_bytes += other.direct_bytes;
        self.direct_files += other.direct_files;

        for file in other.top_files {
            insert_top_file(&mut self.top_files, file);
        }
    }
}

fn insert_top_file(files: &mut Vec<FileInfo>, candidate: FileInfo) {
    if files.len() < TOP_FILES_PER_DIRECTORY {
        files.push(candidate);
        return;
    }

    let Some((smallest_index, smallest)) = files
        .iter()
        .enumerate()
        .min_by_key(|(_, file)| file.size)
    else {
        return;
    };

    if candidate.size > smallest.size {
        files[smallest_index] = candidate;
    }
}

impl WorkerState {
    fn new(shared: Arc<SharedScan>, root: PathBuf) -> Self {
        Self {
            shared,
            root,
            directories: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn record_error(&mut self, message: String) {
        self.shared
            .counters
            .errors
            .fetch_add(1, AtomicOrdering::Relaxed);

        if self.errors.len() < MAX_ERROR_SAMPLES {
            self.errors.push(message);
        }
    }

    fn process(&mut self, result: Result<DirEntry, ignore::Error>) {
        let entry_number = self
            .shared
            .counters
            .entries
            .fetch_add(1, AtomicOrdering::Relaxed)
            + 1;

        if entry_number % PROGRESS_INTERVAL == 0 {
            let files = self
                .shared
                .counters
                .files
                .load(AtomicOrdering::Relaxed);
            let directories = self
                .shared
                .counters
                .directories
                .load(AtomicOrdering::Relaxed);

            eprint!(
                "\rScanned {} entries | {} directories | {} files",
                format_number(u128::from(entry_number)),
                format_number(u128::from(directories)),
                format_number(u128::from(files)),
            );
            let _ = io::stderr().flush();
        }

        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                self.record_error(error.to_string());
                return;
            }
        };

        if entry.path_is_symlink() {
            self.shared
                .counters
                .skipped
                .fetch_add(1, AtomicOrdering::Relaxed);
            return;
        }

        let path = entry.path();
        let Some(file_type) = entry.file_type() else {
            self.shared
                .counters
                .skipped
                .fetch_add(1, AtomicOrdering::Relaxed);
            return;
        };

        if file_type.is_dir() {
            self.directories.entry(path.to_path_buf()).or_default();
            self.shared
                .counters
                .directories
                .fetch_add(1, AtomicOrdering::Relaxed);
            return;
        }

        if !file_type.is_file() {
            self.shared
                .counters
                .skipped
                .fetch_add(1, AtomicOrdering::Relaxed);
            return;
        }

        self.shared
            .counters
            .files
            .fetch_add(1, AtomicOrdering::Relaxed);

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                self.record_error(error.to_string());
                return;
            }
        };

        let parent = path.parent().unwrap_or(&self.root);
        self.directories
            .entry(parent.to_path_buf())
            .or_default()
            .add_file(path, metadata.len());
    }
}

impl Drop for WorkerState {
    fn drop(&mut self) {
        let shard = WorkerShard {
            directories: std::mem::take(&mut self.directories),
            errors: std::mem::take(&mut self.errors),
        };

        let mut shards = self
            .shared
            .shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        shards.push(shard);
    }
}

fn main() {
    let config = match parse_args() {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(error) => {
            eprintln!("Error: {error}\n\nRun with --help for usage.");
            process::exit(2);
        }
    };

    if let Err(error) = run(config) {
        eprintln!("\nFatal error: {error}");
        process::exit(1);
    }
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut root: Option<PathBuf> = None;
    let mut excludes = Vec::new();
    let mut threads = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(8)
        .clamp(4, 16);
    let mut page_size = 40usize;

    let args: Vec<String> = env::args().skip(1).collect();
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];

        if arg == "--help" || arg == "-h" {
            print_help();
            return Ok(None);
        }

        if arg == "--exclude" {
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "Missing value after --exclude".to_string())?;
            excludes.push(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--exclude=") {
            excludes.push(PathBuf::from(value));
        } else if arg == "--threads" {
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "Missing value after --threads".to_string())?;
            threads = parse_usize(value, "threads", 1, 64)?;
        } else if let Some(value) = arg.strip_prefix("--threads=") {
            threads = parse_usize(value, "threads", 1, 64)?;
        } else if arg == "--limit" {
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "Missing value after --limit".to_string())?;
            page_size = parse_usize(value, "limit", 10, 200)?;
        } else if let Some(value) = arg.strip_prefix("--limit=") {
            page_size = parse_usize(value, "limit", 10, 200)?;
        } else if arg.starts_with('-') {
            return Err(format!("Unknown option: {arg}"));
        } else if root.is_none() {
            root = Some(PathBuf::from(arg));
        } else {
            return Err(format!("Unexpected argument: {arg}"));
        }

        index += 1;
    }

    let root = root.unwrap_or_else(default_root);
    let root = fs::canonicalize(&root)
        .map_err(|error| format!("Cannot access root '{}': {error}", root.display()))?;

    if !root.is_dir() {
        return Err(format!("Root is not a directory: {}", root.display()));
    }

    let mut resolved_excludes = Vec::with_capacity(excludes.len());
    for exclude in excludes {
        let candidate = if exclude.is_absolute() {
            exclude
        } else {
            root.join(exclude)
        };

        let resolved = fs::canonicalize(&candidate).unwrap_or(candidate);
        resolved_excludes.push(resolved);
    }

    for exclude in &resolved_excludes {
        if path_is_same_or_descendant(&root, exclude) {
            return Err(format!(
                "The selected root is inside an excluded path: {}",
                exclude.display()
            ));
        }
    }

    Ok(Some(Config {
        root,
        excludes: resolved_excludes,
        threads,
        page_size,
    }))
}

fn default_root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\")
    } else {
        PathBuf::from(".")
    }
}

fn parse_usize(value: &str, name: &str, min: usize, max: usize) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("Invalid {name} value: {value}"))?;

    if !(min..=max).contains(&parsed) {
        return Err(format!("{name} must be between {min} and {max}"));
    }

    Ok(parsed)
}

fn print_help() {
    println!(
        "Disk Analyzer - parallel disk usage analyzer\n\n\
         USAGE:\n\
           disk-analyzer.exe [ROOT] [OPTIONS]\n\n\
         OPTIONS:\n\
           --exclude PATH   Exclude a directory or file (repeatable)\n\
           --threads N      Worker threads (1..64)\n\
           --limit N        Directories per page (10..200)\n\
           -h, --help       Show this help\n\n\
         EXAMPLES:\n\
           disk-analyzer.exe C:\\                    \n\
           disk-analyzer.exe C:\\ --exclude C:\\Windows\n\
           disk-analyzer.exe C:\\ --threads 12 --limit 60\n"
    );
}

fn run(config: Config) -> Result<(), String> {
    let result = scan(&config)?;

    println!("\n\nScan completed.");
    println!("Root:          {}", display_path(&config.root));
    println!("Directories:   {}", format_number(u128::from(result.directories)));
    println!("Files:         {}", format_number(u128::from(result.files)));
    println!("Entries:       {}", format_number(u128::from(result.entries)));
    println!("Excluded:      {}", format_number(u128::from(result.excluded)));
    println!("Skipped:       {}", format_number(u128::from(result.skipped)));
    println!("Errors:        {}", format_number(u128::from(result.errors)));
    println!(
        "Logical size:  {}",
        format_bytes(result.nodes[result.root_index].total_bytes)
    );

    if !result.error_samples.is_empty() {
        println!("\nSome scan errors (up to {MAX_ERROR_SAMPLES}):");
        for error in &result.error_samples {
            println!("  - {error}");
        }
    }

    if result.errors > 0 {
        println!(
            "\nWARNING: Some paths could not be read. Reported totals may be incomplete."
        );
    }

    interactive_explorer(&result, config.page_size);
    Ok(())
}

fn scan(config: &Config) -> Result<ScanResult, String> {
    let shared = Arc::new(SharedScan {
        shards: Mutex::new(Vec::new()),
        counters: Counters::default(),
    });

    let excludes = Arc::new(config.excludes.clone());
    let excluded_counter = Arc::new(AtomicU64::new(0));

    println!("Starting parallel scan...");
    println!("Root:        {}", display_path(&config.root));
    println!("Threads:     {}", config.threads);
    println!("Exclusions:  {}", config.excludes.len());
    println!();

    let mut builder = WalkBuilder::new(&config.root);
    builder
        .standard_filters(false)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .follow_links(false)
        .same_file_system(true)
        .threads(config.threads);

    let filter_excludes = Arc::clone(&excludes);
    let filter_excluded_counter = Arc::clone(&excluded_counter);

    builder.filter_entry(move |entry| {
        let excluded = filter_excludes
            .iter()
            .any(|path| path_is_same_or_descendant(entry.path(), path));

        if excluded {
            filter_excluded_counter.fetch_add(1, AtomicOrdering::Relaxed);
        }

        !excluded
    });

    let root_for_workers = config.root.clone();
    let shared_for_workers = Arc::clone(&shared);

    builder.build_parallel().run(move || {
        let mut state = WorkerState::new(
            Arc::clone(&shared_for_workers),
            root_for_workers.clone(),
        );

        Box::new(move |entry| {
            state.process(entry);
            WalkState::Continue
        })
    });

    let counters = &shared.counters;
    let entries = counters.entries.load(AtomicOrdering::Relaxed);
    let directories = counters.directories.load(AtomicOrdering::Relaxed);
    let files = counters.files.load(AtomicOrdering::Relaxed);
    let skipped = counters.skipped.load(AtomicOrdering::Relaxed);
    let errors = counters.errors.load(AtomicOrdering::Relaxed);
    let excluded = excluded_counter.load(AtomicOrdering::Relaxed);

    eprint!("\rScanned {} entries.                            \n", format_number(u128::from(entries)));

    let mut directory_stats = HashMap::<PathBuf, DirectoryStats>::new();
    let mut error_samples = Vec::new();

    let shards = {
        let mut guard = shared
            .shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *guard)
    };

    for shard in shards {
        for (path, stats) in shard.directories {
            directory_stats.entry(path).or_default().merge(stats);
        }

        for error in shard.errors {
            if error_samples.len() < MAX_ERROR_SAMPLES {
                error_samples.push(error);
            }
        }
    }

    directory_stats.entry(config.root.clone()).or_default();

    let (nodes, root_index) = build_directory_tree(config, directory_stats)?;

    Ok(ScanResult {
        nodes,
        root_index,
        entries,
        directories,
        files,
        skipped,
        errors,
        excluded,
        error_samples,
    })
}

fn build_directory_tree(
    config: &Config,
    directory_stats: HashMap<PathBuf, DirectoryStats>,
) -> Result<(Vec<DirectoryNode>, usize), String> {
    let mut records: Vec<(PathBuf, DirectoryStats)> = directory_stats.into_iter().collect();

    records.sort_by(|(path_a, _), (path_b, _)| {
        let depth_a = relative_depth(path_a, &config.root);
        let depth_b = relative_depth(path_b, &config.root);
        depth_a.cmp(&depth_b).then_with(|| path_a.cmp(path_b))
    });

    let mut nodes = Vec::with_capacity(records.len());
    let mut path_to_index = HashMap::<PathBuf, usize>::with_capacity(records.len());

    for (path, stats) in records {
        let index = nodes.len();
        let depth = relative_depth(&path, &config.root);

        path_to_index.insert(path.clone(), index);
        nodes.push(DirectoryNode {
            path,
            parent: None,
            children: Vec::new(),
            direct_bytes: stats.direct_bytes,
            total_bytes: stats.direct_bytes,
            direct_files: stats.direct_files,
            total_files: stats.direct_files,
            top_files: stats.top_files,
            depth,
        });
    }

    let root_index = *path_to_index
        .get(&config.root)
        .ok_or_else(|| "Internal error: root directory was not indexed".to_string())?;

    for index in 0..nodes.len() {
        if index == root_index {
            continue;
        }

        let parent_index = nodes[index]
            .path
            .parent()
            .and_then(|parent| path_to_index.get(parent).copied())
            .unwrap_or(root_index);

        nodes[index].parent = Some(parent_index);
        nodes[parent_index].children.push(index);
    }

    let mut bottom_up: Vec<usize> = (0..nodes.len()).collect();
    bottom_up.sort_unstable_by_key(|index| std::cmp::Reverse(nodes[*index].depth));

    for index in bottom_up {
        let Some(parent_index) = nodes[index].parent else {
            continue;
        };

        let child_bytes = nodes[index].total_bytes;
        let child_files = nodes[index].total_files;

        nodes[parent_index].total_bytes += child_bytes;
        nodes[parent_index].total_files += child_files;
    }

    Ok((nodes, root_index))
}

fn relative_depth(path: &Path, root: &Path) -> usize {
    path.strip_prefix(root)
        .map(|relative| relative.components().count())
        .unwrap_or(0)
}

fn path_is_same_or_descendant(candidate: &Path, base: &Path) -> bool {
    let mut candidate_components = candidate.components();

    for base_component in base.components() {
        let Some(candidate_component) = candidate_components.next() else {
            return false;
        };

        if !components_equal_ignore_case(candidate_component, base_component) {
            return false;
        }
    }

    true
}

fn components_equal_ignore_case(left: Component<'_>, right: Component<'_>) -> bool {
    match (left, right) {
        (Component::Prefix(left), Component::Prefix(right)) => left
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy()),
        (Component::RootDir, Component::RootDir) => true,
        (Component::CurDir, Component::CurDir) => true,
        (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(left), Component::Normal(right)) => left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy()),
        _ => false,
    }
}

fn interactive_explorer(result: &ScanResult, page_size: usize) {
    let mut current = result.root_index;
    let mut page = 0usize;
    let stdin = io::stdin();
    let mut input = String::new();

    loop {
        print_directory(result, current, page, page_size);
        print!("\nSelect: ");
        let _ = io::stdout().flush();

        input.clear();
        match stdin.read_line(&mut input) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("Input error: {error}");
                break;
            }
        }

        let command = input.trim();

        if command.eq_ignore_ascii_case("q") {
            break;
        }

        if command.eq_ignore_ascii_case("r") {
            continue;
        }

        if command == ".." {
            if let Some(parent) = result.nodes[current].parent {
                current = parent;
                page = 0;
            }
            continue;
        }

        if command.eq_ignore_ascii_case("n") {
            let count = result.nodes[current].children.len();
            let max_page = count.saturating_sub(1) / page_size;
            if page < max_page {
                page += 1;
            }
            continue;
        }

        if command.eq_ignore_ascii_case("p") {
            page = page.saturating_sub(1);
            continue;
        }

        let selection = match command.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                println!("Invalid command.");
                continue;
            }
        };

        let children = sorted_children(result, current);
        let start = page.saturating_mul(page_size);
        let Some(selected) = children.get(start + selection - 1) else {
            println!("Selection is outside the current page.");
            continue;
        };

        current = *selected;
        page = 0;
    }
}

fn sorted_children(result: &ScanResult, directory_index: usize) -> Vec<usize> {
    let mut children = result.nodes[directory_index].children.clone();

    children.sort_unstable_by(|left, right| {
        result.nodes[*right]
            .total_bytes
            .cmp(&result.nodes[*left].total_bytes)
            .then_with(|| result.nodes[*left].path.cmp(&result.nodes[*right].path))
    });

    children
}

fn print_directory(
    result: &ScanResult,
    directory_index: usize,
    page: usize,
    page_size: usize,
) {
    let directory = &result.nodes[directory_index];
    let children = sorted_children(result, directory_index);
    let total_children = children.len();
    let max_page = total_children.saturating_sub(1) / page_size;
    let current_page = page.min(max_page);
    let start = current_page.saturating_mul(page_size);
    let end = (start + page_size).min(total_children);

    println!("\n{}", "=".repeat(110));
    println!("DISK ANALYZER");
    println!("{}", "=".repeat(110));
    println!("Path:          {}", display_path(&directory.path));
    println!("Total size:    {}", format_bytes(directory.total_bytes));
    println!("Direct files:  {}", format_bytes(directory.direct_bytes));
    println!("Files:         {} total | {} direct", format_number(u128::from(directory.total_files)), format_number(u128::from(directory.direct_files)));
    println!("Directories:   {}", format_number(total_children as u128));

    println!("\nTOP DIRECT FILES");
    println!("{}", "-".repeat(110));

    let mut top_files = directory.top_files.clone();
    top_files.sort_unstable_by(|left, right| {
        right.size.cmp(&left.size).then_with(|| left.path.cmp(&right.path))
    });

    if top_files.is_empty() {
        println!("No direct files.");
    } else {
        for file in top_files.iter().take(TOP_FILES_PER_DIRECTORY) {
            println!("{:>12}  {}", format_bytes(u128::from(file.size)), display_path(&file.path));
        }
    }

    println!("\nSUBDIRECTORIES (sorted by total size)");
    println!("{}", "-".repeat(110));
    println!("{:<5} {:>12} {:>8}  {}", "#", "SIZE", "SHARE", "DIRECTORY");
    println!("{}", "-".repeat(110));

    if start >= total_children {
        println!("No subdirectories.");
    } else {
        for (visible_index, child_index) in children[start..end].iter().enumerate() {
            let child = &result.nodes[*child_index];
            let share = if directory.total_bytes == 0 {
                0.0
            } else {
                (child.total_bytes as f64 / directory.total_bytes as f64) * 100.0
            };

            println!(
                "{:<5} {:>12} {:>7.2}%  {}",
                visible_index + 1,
                format_bytes(child.total_bytes),
                share,
                child.path.file_name().unwrap_or(child.path.as_os_str()).to_string_lossy()
            );
        }
    }

    println!("{}", "-".repeat(110));
    if total_children > page_size {
        println!("Page {} of {}", current_page + 1, max_page + 1);
    }
    println!("Commands: number=open | ..=parent | n=next page | p=previous page | r=redraw | q=quit");
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();

    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{}", rest);
    }

    if let Some(rest) = value.strip_prefix(r"\\?\") {
        return rest.to_string();
    }

    value.into_owned()
}

fn format_bytes(bytes: u128) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

    let mut value = bytes as f64;
    let mut unit = 0usize;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn format_number(value: u128) -> String {
    let source = value.to_string();
    let mut output = String::with_capacity(source.len() + source.len() / 3);

    for (index, character) in source.chars().enumerate() {
        if index > 0 && (source.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(character);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting_adds_group_separators() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(1234), "1,234");
        assert_eq!(format_number(123456789), "123,456,789");
    }

    #[test]
    fn byte_formatting_uses_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
    }

    #[test]
    fn top_file_list_keeps_largest_items() {
        let mut files = Vec::new();
        for size in 1..=(TOP_FILES_PER_DIRECTORY as u64 + 5) {
            insert_top_file(
                &mut files,
                FileInfo {
                    path: PathBuf::from(format!("file-{size}")),
                    size,
                },
            );
        }

        assert_eq!(files.len(), TOP_FILES_PER_DIRECTORY);
        assert!(files.iter().all(|file| file.size > 5));
    }

    #[cfg(windows)]
    #[test]
    fn exclude_matching_is_component_aware_and_case_insensitive() {
        assert!(path_is_same_or_descendant(
            Path::new(r"C:\Windows\System32"),
            Path::new(r"c:\windows")
        ));
        assert!(!path_is_same_or_descendant(
            Path::new(r"C:\Windows.old"),
            Path::new(r"C:\Windows")
        ));
    }
}
