use super::{
    build_directory_tree, path_is_same_or_descendant, Config, DirectoryStats, ScanResult,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0000_0200;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
const FILE_INFO_BY_HANDLE_CLASS_STANDARD: i32 = 1;
const FILE_ID_TYPE_FILE_ID: u32 = 0;
const FILE_ID_TYPE_EXTENDED: u32 = 2;
const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
const ERROR_HANDLE_EOF: u32 = 38;
const MFT_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const MAX_ERROR_SAMPLES: usize = 50;

type Handle = *mut c_void;

#[link(name = "Kernel32")]
extern "system" {
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: Handle,
    ) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
    fn SetLastError(error: u32);
    fn GetCompressedFileSizeW(file_name: *const u16, file_size_high: *mut u32) -> u32;
    fn DeviceIoControl(
        device: Handle,
        control_code: u32,
        input_buffer: *mut c_void,
        input_buffer_size: u32,
        output_buffer: *mut c_void,
        output_buffer_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn OpenFileById(
        volume_hint: Handle,
        file_id: *const FileIdDescriptor,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        flags_and_attributes: u32,
    ) -> Handle;
    fn GetFileInformationByHandle(
        file: Handle,
        information: *mut ByHandleFileInformation,
    ) -> i32;
    fn GetFileInformationByHandleEx(
        file: Handle,
        information_class: i32,
        information: *mut c_void,
        buffer_size: u32,
    ) -> i32;
    fn GetVolumeInformationW(
        root_path_name: *const u16,
        volume_name_buffer: *mut u16,
        volume_name_size: u32,
        volume_serial_number: *mut u32,
        maximum_component_length: *mut u32,
        file_system_flags: *mut u32,
        file_system_name_buffer: *mut u16,
        file_system_name_size: u32,
    ) -> i32;
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum FileId {
    V2(u64),
    V3([u8; 16]),
}

impl FileId {
    fn display(self) -> String {
        match self {
            Self::V2(value) => format!("{value:016X}"),
            Self::V3(bytes) => bytes.iter().map(|byte| format!("{byte:02X}")).collect(),
        }
    }

    fn descriptor(self) -> FileIdDescriptor {
        match self {
            Self::V2(value) => FileIdDescriptor {
                size: size_of::<FileIdDescriptor>() as u32,
                kind: FILE_ID_TYPE_FILE_ID,
                low: value,
                high: 0,
            },
            Self::V3(bytes) => FileIdDescriptor {
                size: size_of::<FileIdDescriptor>() as u32,
                kind: FILE_ID_TYPE_EXTENDED,
                low: u64::from_ne_bytes(bytes[0..8].try_into().unwrap()),
                high: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            },
        }
    }
}

#[repr(C)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C, align(8))]
struct FileIdDescriptor {
    size: u32,
    kind: u32,
    low: u64,
    high: u64,
}

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
#[derive(Default)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: FileTime,
    last_access_time: FileTime,
    last_write_time: FileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[repr(C)]
#[derive(Default)]
struct FileStandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
    padding: [u8; 2],
}

#[derive(Clone)]
struct MftRecord {
    file_id: FileId,
    parent_id: FileId,
    name: OsString,
    is_directory: bool,
    is_reparse_point: bool,
}

struct FileCandidate {
    record_index: usize,
}

struct QueriedFile {
    record_index: usize,
    logical_size: u64,
    allocated_size: u64,
    links: u32,
}

struct QueriedDirectory {
    path: PathBuf,
    allocated_size: u64,
}

#[derive(Default)]
struct DirectoryQueryWorkerResult {
    directories: Vec<QueriedDirectory>,
    errors: Vec<String>,
    error_count: u64,
}

#[derive(Default)]
struct QueryWorkerResult {
    files: Vec<QueriedFile>,
    errors: Vec<String>,
    error_count: u64,
    hard_linked_files: u64,
}

struct OwnedHandle(Handle);

impl OwnedHandle {
    fn new(handle: Handle) -> Result<Self, String> {
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(last_error("Create/open handle failed"));
        }
        Ok(Self(handle))
    }

    fn raw(&self) -> Handle {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(super) fn scan(config: &Config) -> Result<ScanResult, String> {
    let (device_path, volume_root) = volume_paths(&config.root)?;
    ensure_ntfs(&volume_root)?;

    println!("Starting native NTFS MFT scan...");
    println!("Volume:      {device_path}");
    println!("Root:        {}", super::display_path(&config.root));
    println!("Threads:     {}", config.threads);
    println!("Exclusions:  {}", config.excludes.len());
    println!("Phase 1/3: Enumerating NTFS MFT records...");

    let volume = open_volume(&device_path)?;
    let root_id = query_path_file_id(&config.root)?;
    let records = enumerate_mft(volume.raw())?;

    if records.is_empty() {
        return Err("The NTFS MFT enumeration returned no file records.".to_string());
    }

    let mut records_by_parent: HashMap<FileId, Vec<usize>> =
        HashMap::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        records_by_parent
            .entry(record.parent_id)
            .or_default()
            .push(index);
    }

    println!(
        "MFT records: {}. Building the selected directory subtree...",
        super::format_number(records.len() as u128)
    );

    let mut directory_paths = HashMap::<FileId, PathBuf>::new();
    let mut directory_stats = HashMap::<PathBuf, DirectoryStats>::new();
    let mut queue = VecDeque::<(FileId, PathBuf)>::new();
    let mut candidates = Vec::<FileCandidate>::new();
    let mut skipped = 0u64;
    let mut excluded = 0u64;

    directory_paths.insert(root_id, config.root.clone());
    directory_stats.entry(config.root.clone()).or_default();
    queue.push_back((root_id, config.root.clone()));

    while let Some((directory_id, directory_path)) = queue.pop_front() {
        let Some(child_indexes) = records_by_parent.remove(&directory_id) else {
            continue;
        };

        for child_index in child_indexes {
            let record = &records[child_index];

            if record.file_id == directory_id
                || record.name.as_os_str() == OsStr::new(".")
                || record.name.as_os_str() == OsStr::new("..")
            {
                skipped += 1;
                continue;
            }

            let full_path = directory_path.join(&record.name);

            if config
                .excludes
                .iter()
                .any(|exclude| path_is_same_or_descendant(&full_path, exclude))
            {
                excluded += 1;
                continue;
            }

            if record.is_reparse_point {
                skipped += 1;
                continue;
            }

            if record.is_directory {
                if directory_paths.contains_key(&record.file_id) {
                    skipped += 1;
                    continue;
                }
                directory_paths.insert(record.file_id, full_path.clone());
                directory_stats.entry(full_path.clone()).or_default();
                queue.push_back((record.file_id, full_path));
            } else {
                candidates.push(FileCandidate {
                    record_index: child_index,
                });
            }
        }
    }

    if !directory_paths.contains_key(&root_id) {
        return Err("Unable to establish the selected root directory in the MFT.".to_string());
    }

    println!(
        "Selected subtree: {} directories, {} files, {} excluded, {} skipped reparse points.",
        super::format_number(directory_paths.len() as u128),
        super::format_number(candidates.len() as u128),
        super::format_number(u128::from(excluded)),
        super::format_number(u128::from(skipped)),
    );
    println!("Phase 2/3: Querying directory and file allocation metadata...");

    let queried_directories = query_directories_parallel(
        &device_path,
        &directory_paths,
        config.threads,
    )?;
    let mut error_samples = Vec::new();
    let mut query_errors = 0u64;

    for worker in queried_directories {
        query_errors += worker.error_count;
        for error in worker.errors {
            if error_samples.len() < MAX_ERROR_SAMPLES {
                error_samples.push(error);
            }
        }
        for directory in worker.directories {
            directory_stats
                .entry(directory.path)
                .or_default()
                .add_directory_allocation(directory.allocated_size);
        }
    }

    let queried = query_files_parallel(
        &device_path,
        &candidates,
        &records,
        &directory_paths,
        config.threads,
    )?;
    let mut file_query_errors = 0u64;
    let mut hard_linked_files = 0u64;

    for worker in queried {
        query_errors += worker.error_count;
        file_query_errors += worker.error_count;
        hard_linked_files += worker.hard_linked_files;

        for error in worker.errors {
            if error_samples.len() < MAX_ERROR_SAMPLES {
                error_samples.push(error);
            }
        }

        for file in worker.files {
            let record = &records[file.record_index];
            let Some(parent_path) = directory_paths.get(&record.parent_id) else {
                skipped += 1;
                continue;
            };

            let full_path = parent_path.join(&record.name);
            directory_stats
                .entry(parent_path.clone())
                .or_default()
                .add_file_with_metadata(
                    &full_path,
                    file.logical_size,
                    file.allocated_size,
                    Some(record.file_id.display()),
                    file.links,
                );
        }
    }

    if !candidates.is_empty() && file_query_errors as usize == candidates.len() {
        return Err(
            "MFT records were enumerated, but every file metadata query failed. Run elevated or use --engine walk.".to_string(),
        );
    }

    println!("Phase 3/3: Aggregating directory sizes...");

    let (nodes, root_index) = build_directory_tree(config, directory_stats)?;
    let directories = nodes.len() as u64;
    let files = candidates.len() as u64;
    let entries = records.len() as u64;

    Ok(ScanResult {
        nodes,
        root_index,
        entries,
        directories,
        files,
        skipped,
        errors: query_errors,
        excluded,
        error_samples,
        engine: "Native NTFS MFT (USN + OpenFileById)".to_string(),
        allocated_available: true,
        hard_linked_files,
    })
}

fn volume_paths(root: &Path) -> Result<(String, String), String> {
    let value = root.to_string_lossy();
    let normalized = value.strip_prefix(r"\\?\").unwrap_or(value.as_ref());
    let bytes = normalized.as_bytes();

    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return Err(
            "The MFT engine currently supports local drive-letter volumes only (for example C:\\)." 
                .to_string(),
        );
    }

    let drive = (bytes[0] as char).to_ascii_uppercase();
    Ok((format!("\\\\.\\{drive}:"), format!("{drive}:\\")))
}

fn ensure_ntfs(volume_root: &str) -> Result<(), String> {
    let root_wide = to_wide(volume_root);
    let mut file_system = [0u16; 32];

    let ok = unsafe {
        GetVolumeInformationW(
            root_wide.as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            file_system.as_mut_ptr(),
            file_system.len() as u32,
        )
    };

    if ok == 0 {
        return Err(last_error("Unable to query volume filesystem type"));
    }

    let file_system_name = String::from_utf16_lossy(
        &file_system[..file_system.iter().position(|value| *value == 0).unwrap_or(file_system.len())],
    );

    if !file_system_name.eq_ignore_ascii_case("NTFS") {
        return Err(format!(
            "The native MFT engine requires NTFS; this volume reports '{file_system_name}'."
        ));
    }

    Ok(())
}

fn open_volume(device_path: &str) -> Result<OwnedHandle, String> {
    let path = to_wide(device_path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    OwnedHandle::new(handle)
}

fn query_path_file_id(path: &Path) -> Result<FileId, String> {
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    let handle = OwnedHandle::new(handle)?;

    let mut information = ByHandleFileInformation::default();
    let ok = unsafe { GetFileInformationByHandle(handle.raw(), &mut information) };
    if ok == 0 {
        return Err(last_error("Unable to retrieve the selected root File ID"));
    }

    let file_id = (u64::from(information.file_index_high) << 32)
        | u64::from(information.file_index_low);
    Ok(FileId::V2(file_id))
}

fn enumerate_mft(volume: Handle) -> Result<Vec<MftRecord>, String> {
    let mut start_file_reference_number = 0u64;
    let mut output = vec![0u8; MFT_BUFFER_SIZE];
    let mut records = Vec::<MftRecord>::new();
    let mut unique_ids = HashSet::<FileId>::new();
    let mut batches = 0u64;

    loop {
        let mut input = MftEnumDataV0 {
            start_file_reference_number,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        let mut bytes_returned = 0u32;
        let success = unsafe {
            DeviceIoControl(
                volume,
                FSCTL_ENUM_USN_DATA,
                (&mut input as *mut MftEnumDataV0).cast(),
                size_of::<MftEnumDataV0>() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut bytes_returned,
                ptr::null_mut(),
            )
        };

        if success == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_HANDLE_EOF {
                break;
            }
            return Err(format!(
                "FSCTL_ENUM_USN_DATA failed: {}",
                io::Error::from_raw_os_error(code)
            ));
        }

        let returned = bytes_returned as usize;
        if returned < 8 {
            break;
        }

        let next_start = read_u64(&output, 0)?;
        batches += 1;
        if batches % 16 == 0 {
            eprint!(
                "\rMFT batches: {} | unique records: {}",
                super::format_number(u128::from(batches)),
                super::format_number(records.len() as u128)
            );
        }
        let mut offset = 8usize;

        while offset + 8 <= returned {
            let record_length = read_u32(&output, offset)? as usize;
            if record_length == 0 {
                break;
            }
            if record_length < 60 || offset + record_length > returned {
                return Err("The NTFS MFT response contained a malformed USN record.".to_string());
            }

            let major_version = read_u16(&output, offset + 4)?;
            let record = match major_version {
                2 => parse_v2_record(&output[offset..offset + record_length])?,
                3 => parse_v3_record(&output[offset..offset + record_length])?,
                version => {
                    return Err(format!(
                        "Unsupported USN record version {version}; refusing an incomplete MFT scan."
                    ));
                }
            };

            if unique_ids.insert(record.file_id) {
                records.push(record);
            }

            offset += record_length;
        }

        if next_start <= start_file_reference_number || returned == 8 {
            break;
        }
        start_file_reference_number = next_start;
    }

    if batches >= 16 {
        eprintln!();
    }
    Ok(records)
}

fn parse_v2_record(buffer: &[u8]) -> Result<MftRecord, String> {
    if buffer.len() < 60 {
        return Err("Truncated USN_RECORD_V2 header.".to_string());
    }

    let name_length = read_u16(buffer, 56)? as usize;
    let name_offset = read_u16(buffer, 58)? as usize;
    let name = parse_record_name(buffer, name_offset, name_length, 60)?;

    Ok(MftRecord {
        file_id: FileId::V2(read_u64(buffer, 8)?),
        parent_id: FileId::V2(read_u64(buffer, 16)?),
        name,
        is_directory: read_u32(buffer, 52)? & FILE_ATTRIBUTE_DIRECTORY != 0,
        is_reparse_point: read_u32(buffer, 52)? & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    })
}

fn parse_v3_record(buffer: &[u8]) -> Result<MftRecord, String> {
    if buffer.len() < 76 {
        return Err("Truncated USN_RECORD_V3 header.".to_string());
    }

    let name_length = read_u16(buffer, 72)? as usize;
    let name_offset = read_u16(buffer, 74)? as usize;
    let name = parse_record_name(buffer, name_offset, name_length, 76)?;
    let mut file_id = [0u8; 16];
    let mut parent_id = [0u8; 16];
    file_id.copy_from_slice(&buffer[8..24]);
    parent_id.copy_from_slice(&buffer[24..40]);

    Ok(MftRecord {
        file_id: FileId::V3(file_id),
        parent_id: FileId::V3(parent_id),
        name,
        is_directory: read_u32(buffer, 68)? & FILE_ATTRIBUTE_DIRECTORY != 0,
        is_reparse_point: read_u32(buffer, 68)? & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    })
}

fn parse_record_name(
    buffer: &[u8],
    offset: usize,
    length: usize,
    minimum_offset: usize,
) -> Result<OsString, String> {
    if length == 0
        || length % 2 != 0
        || offset < minimum_offset
        || offset.checked_add(length).is_none_or(|end| end > buffer.len())
    {
        return Err("The MFT response contained an invalid UTF-16 filename range.".to_string());
    }

    let words: Vec<u16> = buffer[offset..offset + length]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(OsString::from_wide(&words))
}

fn query_directories_parallel(
    device_path: &str,
    directories: &HashMap<FileId, PathBuf>,
    threads: usize,
) -> Result<Vec<DirectoryQueryWorkerResult>, String> {
    if directories.is_empty() {
        return Ok(Vec::new());
    }

    let candidates: Vec<(FileId, PathBuf)> = directories
        .iter()
        .map(|(file_id, path)| (*file_id, path.clone()))
        .collect();
    let worker_count = threads.max(1).min(candidates.len());
    let chunk_size = candidates.len().div_ceil(worker_count);
    let results = Mutex::new(Vec::<DirectoryQueryWorkerResult>::with_capacity(worker_count));
    let completed = AtomicU64::new(0);

    std::thread::scope(|scope| {
        for chunk in candidates.chunks(chunk_size) {
            let volume_name = device_path.to_string();
            let results_ref = &results;
            let completed_ref = &completed;
            let total = candidates.len();

            scope.spawn(move || {
                let mut output = DirectoryQueryWorkerResult::default();
                let volume = match open_volume(&volume_name) {
                    Ok(handle) => handle,
                    Err(error) => {
                        output.error_count = chunk.len() as u64;
                        output.errors.push(format!(
                            "Worker could not open volume handle; {} directories not queried: {error}",
                            chunk.len()
                        ));
                        results_ref
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(output);
                        return;
                    }
                };

                for (file_id, path) in chunk {
                    match query_directory_by_id(volume.raw(), *file_id) {
                        Ok(allocated_size) => output.directories.push(QueriedDirectory {
                            path: path.clone(),
                            allocated_size,
                        }),
                        Err(error) => {
                            output.error_count += 1;
                            if output.errors.len() < MAX_ERROR_SAMPLES {
                                output.errors.push(format!(
                                    "Directory File ID {} ({}): {error}",
                                    file_id.display(),
                                    super::display_path(path)
                                ));
                            }
                        }
                    }

                    let done = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 50_000 == 0 {
                        eprint!(
                            "\rDirectory metadata queried: {} / {}",
                            super::format_number(u128::from(done)),
                            super::format_number(total as u128)
                        );
                    }
                }

                results_ref
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(output);
            });
        }
    });

    if completed.load(Ordering::Relaxed) >= 50_000 {
        eprintln!();
    }

    Ok(results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner()))
}

fn query_directory_by_id(volume: Handle, file_id: FileId) -> Result<u64, String> {
    let descriptor = file_id.descriptor();
    let handle = unsafe {
        OpenFileById(
            volume,
            &descriptor,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    let handle = OwnedHandle::new(handle)?;

    let mut information = FileStandardInfo::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.raw(),
            FILE_INFO_BY_HANDLE_CLASS_STANDARD,
            (&mut information as *mut FileStandardInfo).cast(),
            size_of::<FileStandardInfo>() as u32,
        )
    };

    if ok == 0 {
        return Err(last_error("GetFileInformationByHandleEx(FileStandardInfo) failed"));
    }

    if information.delete_pending != 0 || information.directory == 0 {
        return Err("The MFT directory entry changed during scanning or is not a directory.".to_string());
    }

    Ok(information.allocation_size.max(0) as u64)
}

fn query_files_parallel(
    device_path: &str,
    candidates: &[FileCandidate],
    records: &[MftRecord],
    directory_paths: &HashMap<FileId, PathBuf>,
    threads: usize,
) -> Result<Vec<QueryWorkerResult>, String> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = threads.max(1).min(candidates.len());
    let chunk_size = candidates.len().div_ceil(worker_count);
    let results = Mutex::new(Vec::<QueryWorkerResult>::with_capacity(worker_count));
    let completed = AtomicU64::new(0);

    std::thread::scope(|scope| {
        for chunk in candidates.chunks(chunk_size) {
            let volume_name = device_path.to_string();
            let results_ref = &results;
            let records_ref = records;
            let directory_paths_ref = directory_paths;
            let completed_ref = &completed;

            scope.spawn(move || {
                let mut output = QueryWorkerResult::default();
                let volume = match open_volume(&volume_name) {
                    Ok(handle) => handle,
                    Err(error) => {
                        output.error_count = chunk.len() as u64;
                        output.errors.push(format!(
                            "Worker could not open volume handle; {} file records not queried: {error}",
                            chunk.len()
                        ));
                        results_ref
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(output);
                        return;
                    }
                };

                for candidate in chunk {
                    let record = &records_ref[candidate.record_index];
                    let Some(parent_path) = directory_paths_ref.get(&record.parent_id) else {
                        output.error_count += 1;
                        continue;
                    };
                    let full_path = parent_path.join(&record.name);
                    match query_file_by_id(volume.raw(), record.file_id, &full_path) {
                        Ok(metadata) => {
                            if metadata.links > 1 {
                                output.hard_linked_files += 1;
                            }

                            output.files.push(QueriedFile {
                                record_index: candidate.record_index,
                                logical_size: metadata.logical_size,
                                allocated_size: metadata.allocated_size,
                                links: metadata.links,
                            });
                        }
                        Err(error) => {
                            output.error_count += 1;
                            if output.errors.len() < MAX_ERROR_SAMPLES {
                                output.errors.push(format!(
                                    "File ID {}: {error}",
                                    record.file_id.display()
                                ));
                            }
                        }
                    }

                    let done = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 50_000 == 0 {
                        eprint!(
                            "\rMetadata queried: {} / {}",
                            super::format_number(u128::from(done)),
                            super::format_number(candidates.len() as u128)
                        );
                    }
                }

                results_ref
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(output);
            });
        }
    });

    if completed.load(Ordering::Relaxed) >= 50_000 {
        eprintln!();
    }

    Ok(results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner()))
}

struct FileMetadata {
    logical_size: u64,
    allocated_size: u64,
    links: u32,
}

fn query_file_by_id(
    volume: Handle,
    file_id: FileId,
    full_path: &Path,
) -> Result<FileMetadata, String> {
    let descriptor = file_id.descriptor();
    let handle = unsafe {
        OpenFileById(
            volume,
            &descriptor,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    let handle = OwnedHandle::new(handle)?;

    let mut information = FileStandardInfo::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.raw(),
            FILE_INFO_BY_HANDLE_CLASS_STANDARD,
            (&mut information as *mut FileStandardInfo).cast(),
            size_of::<FileStandardInfo>() as u32,
        )
    };

    if ok == 0 {
        return Err(last_error("GetFileInformationByHandleEx(FileStandardInfo) failed"));
    }

    if information.delete_pending != 0 || information.directory != 0 {
        return Err("The MFT entry changed during scanning or is not a regular file.".to_string());
    }

    let mut allocated_size = information.allocation_size.max(0) as u64;
    let mut by_handle = ByHandleFileInformation::default();
    let attributes_ok = unsafe { GetFileInformationByHandle(handle.raw(), &mut by_handle) };
    if attributes_ok == 0 {
        return Err(last_error("GetFileInformationByHandle failed"));
    }

    if by_handle.file_attributes & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED) != 0 {
        let path_wide: Vec<u16> = full_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut high = 0u32;
        unsafe { SetLastError(0) };
        let low = unsafe { GetCompressedFileSizeW(path_wide.as_ptr(), &mut high) };
        if low == u32::MAX {
            let code = unsafe { GetLastError() };
            if code != 0 {
                return Err(format!(
                    "GetCompressedFileSizeW failed: {} (Windows error {code})",
                    io::Error::from_raw_os_error(code as i32)
                ));
            }
        }
        allocated_size = (u64::from(high) << 32) | u64::from(low);
    }

    Ok(FileMetadata {
        logical_size: information.end_of_file.max(0) as u64,
        allocated_size,
        links: information.number_of_links.max(1),
    })
}

fn read_u16(buffer: &[u8], offset: usize) -> Result<u16, String> {
    let bytes = buffer
        .get(offset..offset + 2)
        .ok_or_else(|| "Unexpected end of MFT response while reading u16.".to_string())?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(buffer: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = buffer
        .get(offset..offset + 4)
        .ok_or_else(|| "Unexpected end of MFT response while reading u32.".to_string())?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u64(buffer: &[u8], offset: usize) -> Result<u64, String> {
    let bytes = buffer
        .get(offset..offset + 8)
        .ok_or_else(|| "Unexpected end of MFT response while reading u64.".to_string())?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn last_error(context: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{context}: {} (Windows error {code})", io::Error::from_raw_os_error(code as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_usn_record_and_unicode_name() {
        let name: Vec<u16> = "資料.bin".encode_utf16().collect();
        let record_len = 60 + name.len() * 2;
        let mut bytes = vec![0u8; record_len];
        bytes[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&123u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&5u64.to_le_bytes());
        bytes[52..56].copy_from_slice(&0u32.to_le_bytes());
        bytes[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (index, unit) in name.iter().enumerate() {
            bytes[60 + index * 2..62 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }

        let parsed = parse_v2_record(&bytes).unwrap();
        assert_eq!(parsed.file_id, FileId::V2(123));
        assert_eq!(parsed.parent_id, FileId::V2(5));
        assert_eq!(parsed.name, OsString::from("資料.bin"));
        assert!(!parsed.is_directory);
    }

    #[test]
    fn file_id_descriptor_has_expected_win32_layout() {
        assert_eq!(size_of::<FileIdDescriptor>(), 24);
        assert_eq!(size_of::<FileStandardInfo>(), 24);
        let descriptor = FileId::V2(0x1122_3344_5566_7788).descriptor();
        assert_eq!(descriptor.size, 24);
        assert_eq!(descriptor.kind, FILE_ID_TYPE_FILE_ID);
        assert_eq!(descriptor.low, 0x1122_3344_5566_7788);
    }
}
