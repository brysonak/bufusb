/*
    bufusb - Tool for flashing USB drives across platforms
    Copyright (C) 2026 Bryson Kelly

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */


use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use std::fs::File;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct WriteParams {
    pub source: String,
    pub target: String,
    pub block_size: usize,
    pub offset: u64,
}

pub fn validate(params: &WriteParams) -> Result<(u64, File)> {
    info!("Starting validation");
    info!("  source     : {}", params.source);
    info!("  target     : {}", params.target);
    info!("  block_size : {} bytes", params.block_size);
    info!("  offset     : {} bytes", params.offset);

    if params.block_size == 0 {
        bail!("Block size must be greater than zero");
    }
    if params.block_size > 256 * 1024 * 1024 {
        bail!("Block size {} is unreasonably large (max 256 MiB)", params.block_size);
    }

    if params.offset != 0 {
        let sector = crate::writer::get_sector_size(Path::new(&params.target)) as u64;
        if params.offset % sector != 0 {
            bail!(
                "--offset {} is not a multiple of the target's {}-byte sector size",
                params.offset,
                sector
            );
        }
    }

    let source_size = check_source(Path::new(&params.source))?;
    let target_file = open_target_and_check(Path::new(&params.target), source_size, params.offset)?;

    info!("Validation passed, source is {} bytes", source_size);
    Ok((source_size, target_file))
}

fn check_source(source: &Path) -> Result<u64> {
    debug!("Checking source: {}", source.display());

    if !source.exists() {
        bail!("Source file does not exist: {}", source.display());
    }

    let meta = source
        .metadata()
        .with_context(|| format!("Cannot read metadata for source: {}", source.display()))?;

    if !meta.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_block_device() && !meta.file_type().is_char_device() {
                bail!(
                    "Source is not a regular file or block device: {}",
                    source.display()
                );
            }
        }
        #[cfg(not(unix))]
        bail!("Source is not a regular file: {}", source.display());
    }

    let size = file_size(source)
        .with_context(|| format!("Cannot determine size of source: {}", source.display()))?;

    if size == 0 {
        bail!("Source file is empty: {}", source.display());
    }

    info!(
        "Source: {} ({} / {} bytes)",
        source.display(),
        crate::list::human_bytes(size),
        size
    );
    Ok(size)
}

fn open_target_and_check(target: &Path, source_size: u64, offset: u64) -> Result<File> {
    debug!("Opening target for writing: {}", target.display());

    #[cfg(not(windows))]
    if !target.exists() {
        bail!("Target device does not exist: {}", target.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let meta = target
            .metadata()
            .with_context(|| format!("Cannot read metadata for target: {}", target.display()))?;
        if !meta.file_type().is_block_device() {
            warn!(
                "Target '{}' is not a block device, writing to a regular file",
                target.display()
            );
        }
    }

    let file = open_target_file(target).with_context(|| {
        format!(
            "Cannot open target for writing: {} (are you running as root/Administrator?)",
            target.display()
        )
    })?;

    let target_size = device_size(target)?;

    if target_size > 0 {
        let available = target_size.saturating_sub(offset);
        if source_size > available {
            bail!(
                "Source ({}) is larger than available target space ({}, {} offset, {} available)",
                crate::list::human_bytes(source_size),
                crate::list::human_bytes(target_size),
                crate::list::human_bytes(offset),
                crate::list::human_bytes(available),
            );
        }
        info!(
            "Target: {} | capacity {} | {} available after offset",
            target.display(),
            crate::list::human_bytes(target_size),
            crate::list::human_bytes(target_size.saturating_sub(offset)),
        );
    } else {
        warn!("Could not determine target capacity, skipping size check");
    }

    Ok(file)
}

fn file_size(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let meta = path.metadata()?;
        if meta.file_type().is_block_device() {
            return block_device_size(path);
        }
    }
    Ok(path.metadata()?.len())
}

pub fn device_size(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let meta = path.metadata()?;
        if meta.file_type().is_block_device() {
            return block_device_size(path);
        }
        return Ok(meta.len());
    }

    #[cfg(windows)]
    return windows_device_size(path);

    #[cfg(not(any(unix, windows)))]
    Ok(0)
}

#[cfg(target_os = "linux")]
fn block_device_size(path: &Path) -> Result<u64> {
    use std::os::unix::io::AsRawFd;

    let f = File::open(path)?;

    // BLKGETSIZE64 returns device size in bytes. BLKGETSIZE returns 512-byte sector count
    const BLKGETSIZE64: u64 = 0x80081272;

    let mut size: u64 = 0;
    let ret = unsafe { nix::libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut size as *mut u64) };
    if ret == 0 {
        debug!("BLKGETSIZE64 returned {} bytes for {}", size, path.display());
        return Ok(size);
    }

    warn!(
        "BLKGETSIZE64 failed for {}: ret={}, falling back to seek",
        path.display(), ret
    );
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path)?;
    Ok(f.seek(SeekFrom::End(0))?)
}

// On macOS, use DKIOCGETBLOCKCOUNT + DKIOCGETBLOCKSIZE ioctls for block devices
// Seeking to end also works but may return 0 on some macOS versions
#[cfg(target_os = "macos")]
fn block_device_size(path: &Path) -> Result<u64> {
    use std::os::unix::io::AsRawFd;

    let f = File::open(path)?;
    let fd = f.as_raw_fd();

    // DKIOCGETBLOCKCOUNT = 0x40086419, DKIOCGETBLOCKSIZE = 0x40046418
    const DKIOCGETBLOCKCOUNT: u64 = 0x40086419;
    const DKIOCGETBLOCKSIZE: u64 = 0x40046418;

    let mut block_count: u64 = 0;
    let mut block_size: u32 = 0;

    let r1 = unsafe { nix::libc::ioctl(fd, DKIOCGETBLOCKCOUNT as _, &mut block_count as *mut u64) };
    let r2 = unsafe { nix::libc::ioctl(fd, DKIOCGETBLOCKSIZE as _, &mut block_size as *mut u32) };

    if r1 == 0 && r2 == 0 && block_size > 0 {
        return Ok(block_count * block_size as u64);
    }

    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path)?;
    Ok(f.seek(SeekFrom::End(0))?)
}

#[cfg(windows)]
fn windows_device_size(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX};
    use windows::Win32::System::IO::DeviceIoControl;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };

    let handle = match handle {
        Ok(h) if h != INVALID_HANDLE_VALUE => h,
        _ => {
            warn!("Could not open target for size query, skipping size check");
            return Ok(0);
        }
    };

    let mut geom = DISK_GEOMETRY_EX::default();
    let mut bytes_returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            None,
            0,
            Some(&mut geom as *mut _ as *mut _),
            std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    if ok.is_err() || geom.DiskSize <= 0 {
        warn!("IOCTL_DISK_GET_DRIVE_GEOMETRY_EX failed, skipping size check");
        return Ok(0);
    }

    Ok(geom.DiskSize as u64)
}

#[cfg(windows)]
fn open_target_file(path: &Path) -> anyhow::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let flags = FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH;

    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    };

    match handle {
        Ok(h) if h != INVALID_HANDLE_VALUE => Ok(unsafe { File::from_raw_handle(h.0 as *mut _) }),
        _ => anyhow::bail!("CreateFileW failed for {}", path.display()),
    }
}

#[cfg(target_os = "linux")]
fn open_target_file(path: &Path) -> anyhow::Result<File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    Ok(OpenOptions::new()
        .write(true)
        // O_DIRECT differs per arch (0x4000 on x86 is O_DIRECTORY on arm64), take it from libc
        .custom_flags(nix::libc::O_DIRECT | nix::libc::O_SYNC)
        .open(path)?)
}

#[cfg(target_os = "macos")]
fn open_target_file(path: &Path) -> anyhow::Result<File> {
    use std::fs::OpenOptions;
    // F_NOCACHE is applied post-open via fcntl
    let file = OpenOptions::new().write(true).open(path)?;
    macos_set_nocache(&file);
    Ok(file)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn open_target_file(path: &Path) -> anyhow::Result<File> {
    use std::fs::OpenOptions;
    Ok(OpenOptions::new().write(true).open(path)?)
}

#[cfg(target_os = "macos")]
fn macos_set_nocache(file: &File) {
    use std::os::unix::io::AsRawFd;
    // F_NOCACHE disables the unified buffer cache for this fd
    const F_NOCACHE: std::os::raw::c_int = 48;
    unsafe {
        libc_fcntl(file.as_raw_fd(), F_NOCACHE, 1);
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    #[link_name = "fcntl"]
    fn libc_fcntl(fd: std::os::raw::c_int, cmd: std::os::raw::c_int, ...) -> std::os::raw::c_int;
}
