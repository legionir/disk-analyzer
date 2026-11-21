# Disk Analyzer

A Windows-focused disk usage analyzer written in Rust. It includes two scanning engines:

- `mft`: Native NTFS MFT enumeration through `FSCTL_ENUM_USN_DATA`, followed by file-ID-based metadata queries through `OpenFileById` and `GetFileInformationByHandleEx`.
- `walk`: Parallel filesystem traversal using the `ignore` crate. This is the compatibility fallback for non-NTFS volumes and unsupported environments.
- `auto`: Try the native NTFS engine first and fall back to the parallel walker if native enumeration is unavailable. This is the default.

## Build

```powershell
cargo build --release
```

Output:

```text
target\release\disk-analyzer.exe
```

## Run

Native NTFS engine (recommended on local NTFS volumes):

```powershell
.\target\release\disk-analyzer.exe C:\ --engine mft
```

Automatic selection and fallback:

```powershell
.\target\release\disk-analyzer.exe C:\
```

Force the compatibility directory walker:

```powershell
.\target\release\disk-analyzer.exe C:\ --engine walk
```

Exclude directories:

```powershell
.\target\release\disk-analyzer.exe C:\ --exclude C:\Windows --exclude 'C:\$Recycle.Bin'
```

Tune worker count and page size:

```powershell
.\target\release\disk-analyzer.exe C:\ --threads 12 --limit 60
```

## Engines and accounting

### Native NTFS MFT engine

The native engine enumerates NTFS file records and uses volume-scoped file IDs to query file and directory metadata:

- `EndOfFile`: logical file length.
- `AllocationSize`: allocated bytes reported by NTFS for ordinary file streams.
- For sparse or compressed files, `GetCompressedFileSizeW` supplies the actual stored size, rather than the logical length.
- `NumberOfLinks`: number of hard links referring to the same file record.
- Directory `AllocationSize`: included separately as directory-index allocation where Windows reports it.

Each unique file ID is queried and counted once. This prevents hard links from multiplying a file's allocated size. When an item has multiple hard links, the displayed path is the representative name returned by the MFT enumeration; the allocated bytes are attributed to that representative directory for the folder tree. The link count is shown in the top-file list. The engine does not yet enumerate and display every alternate hard-link path. The same physical allocation cannot be independently attributed to every hard-link name without double-counting.

The MFT engine currently supports local drive-letter NTFS volumes (for example `C:\`). It does not support ReFS, network shares, Storage Spaces paths without a drive letter, or arbitrary mounted-folder volume roots. Run from an elevated terminal (Run as Administrator) for the most complete MFT results. Use `--engine auto` to fall back when the native volume operation is denied or unsupported.

### Parallel walker

The walker reports logical file lengths from filesystem metadata. It does not report NTFS allocation size or hard-link identity, so allocated-size columns are unavailable in this mode.

### Accuracy notes

- A scan is a live snapshot assembled while the filesystem may still be changing. Files created, deleted, renamed, or modified during scanning can cause transient omissions or query failures.
- Files that cannot be opened by ID are reported as errors and excluded from size totals. If errors are present, totals are partial.
- Allocated-size totals use `FILE_STANDARD_INFO.AllocationSize` for ordinary files and directory indexes, and `GetCompressedFileSizeW` for sparse or compressed files. These are Windows filesystem-reported byte counts, not a direct physical-sector measurement from the storage device.
- Alternate data streams are not yet enumerated independently. Their allocated bytes may therefore be absent from the per-directory totals.
- NTFS volume-level metadata and space reserved by the filesystem (for example, all internal metadata files and MFT reserved zones) are not guaranteed to be represented as ordinary directory entries. The tool is a file-and-directory usage analyzer, not a complete reconciliation against the volume's free-space bitmap.
- Hard-linked files are counted once by unique file ID. Their one representative path is used for folder attribution; `NumberOfLinks` exposes multi-link files. The other hard-link names are not yet listed individually.
- Reparse points are not followed, preventing junction and symbolic-link loops.
- The MFT API returns file names and parent IDs, while file and directory standard information is queried in parallel. This avoids a recursive directory walk for discovery, but still requires a metadata query for each unique file and directory ID.

## Interactive commands

- `1..N`: open a subdirectory on the current page.
- `..`: go to the parent directory.
- `n`: next page.
- `p`: previous page.
- `r`: redraw.
- `q`: quit.

## GitHub Actions

The workflow is `.github/workflows/build-windows.yml`. It builds and tests `x86_64-pc-windows-msvc`, then uploads the EXE and ZIP as an Actions artifact.

1. Push the project to GitHub.
2. Open **Actions → Build Windows executable**.
3. Download the `disk-analyzer-windows-x86_64` artifact.

To create a GitHub Release, push a version tag:

```bash
git tag v0.2.0
git push origin v0.2.0
```
