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


use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;

use crate::list::human_bytes;
use crate::validate::WriteParams;

pub fn write(params: &WriteParams, source_size: u64, target_file: File) -> Result<()> {
    info!("Beginning write operation");
    info!("  source     : {}", params.source);
    info!("  target     : {}", params.target);
    info!("  block_size : {} bytes", params.block_size);
    info!("  offset     : {} bytes", params.offset);
    info!("  source_size: {} bytes ({})", source_size, human_bytes(source_size));

    let source_path = Path::new(&params.source);

    let mut source_file = File::open(source_path)
        .with_context(|| format!("Failed to open source: {}", params.source))?;

    // Unmount (unix) or lock + dismount (windows) the target's volumes, held until return
    // Windows rejects raw writes inside a mounted volume after the first MiB already landed,
    // on unix a live fs writes its dirty metadata back over the image
    let _prep = crate::copy::prepare_target(Path::new(&params.target))?;

    // Query physical sector size for alignment. With O_DIRECT / FILE_FLAG_NO_BUFFERING,
    // every write must be a multiple of this size in length and start on an aligned offset
    let sector_size = get_sector_size(Path::new(&params.target));
    debug!("Sector size for {}: {} bytes", params.target, sector_size);

    // block_size is already validated to be non-zero. Round it up to a sector boundary
    // so that all full blocks are already aligned, and only the final partial block
    // needs extra handling 
    let aligned_block = round_up(params.block_size, sector_size);

    let mut target = DirectWriter::new(target_file, sector_size);

    if params.offset > 0 {
        info!("Seeking target to offset {} bytes", params.offset);
        target
            .seek(SeekFrom::Start(params.offset))
            .with_context(|| {
                format!("Failed to seek to offset {} on {}", params.offset, params.target)
            })?;
    }

    let pb = build_progress_bar(source_size);

    // Allocate aligned buffer. The extra sector_size headroom covers padding on the
    // final block without needing a separate allocation
    let mut buffer = AlignedBuffer::new(aligned_block + sector_size, sector_size);

    let mut bytes_written: u64 = 0;
    let mut blocks_written: u64 = 0;
    let start = Instant::now();

    loop {
        let bytes_read = source_file
            .read(buffer.as_mut_slice_n(aligned_block))
            .context("Read error from source")?;

        if bytes_read == 0 {
            debug!("EOF reached after {} blocks", blocks_written);
            break;
        }

        let write_len = round_up(bytes_read, sector_size);
        if write_len > bytes_read {
            buffer.zero_range(bytes_read, write_len);
        }

        target
            .write_all(buffer.as_slice_n(write_len))
            .context("Write error to target, device may be full or disconnected")?;

        bytes_written += bytes_read as u64;
        blocks_written += 1;

        pb.set_position(bytes_written);

        debug!("Block {:>6} | {} bytes | {} total", blocks_written, bytes_read, bytes_written);
    }

    pb.set_message("Syncing...");
    target.sync().context("Sync error, data may not have reached the device")?;
    pb.finish_with_message("Done");

    let elapsed = start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64().max(0.001);
    let throughput = bytes_written as f64 / elapsed_secs;

    info!(
        "Write complete: {} in {:.2}s ({}/s) over {} blocks",
        human_bytes(bytes_written),
        elapsed_secs,
        human_bytes(throughput as u64),
        blocks_written,
    );

    println!(
        "\n  Written : {}\n  Time    : {:.2}s\n  Speed   : {}/s\n",
        human_bytes(bytes_written),
        elapsed_secs,
        human_bytes(throughput as u64),
    );

    Ok(())
}

fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

// AlignedBuffer wraps a heap allocation that is guaranteed to start on a
// `align`-byte boundary. O_DIRECT and FILE_FLAG_NO_BUFFERING require that
// the user-space buffer address is aligned to the physical sector size
struct AlignedBuffer {
    // We allocate extra capacity (align - 1 bytes) and offset into it so the
    // first usable byte is aligned. `ptr` points to the first aligned byte
    _storage: Vec<u8>,
    ptr: *mut u8,
    capacity: usize,
}

unsafe impl Send for AlignedBuffer {}

impl AlignedBuffer {
    fn new(capacity: usize, align: usize) -> Self {
        let total = capacity + align;
        let mut storage = vec![0u8; total];
        let raw = storage.as_mut_ptr();
        let offset = raw.align_offset(align);
        let ptr = unsafe { raw.add(offset) };
        Self { _storage: storage, ptr, capacity }
    }

    fn as_mut_slice_n(&mut self, n: usize) -> &mut [u8] {
        assert!(n <= self.capacity);
        unsafe { std::slice::from_raw_parts_mut(self.ptr, n) }
    }

    fn as_slice_n(&self, n: usize) -> &[u8] {
        assert!(n <= self.capacity);
        unsafe { std::slice::from_raw_parts(self.ptr, n) }
    }

    fn zero_range(&mut self, start: usize, end: usize) {
        assert!(end <= self.capacity);
        unsafe {
            std::ptr::write_bytes(self.ptr.add(start), 0, end - start);
        }
    }
}

struct DirectWriter {
    file: File,
    #[allow(dead_code)]
    sector_size: usize,
}

impl DirectWriter {
    fn new(file: File, sector_size: usize) -> Self {
        Self { file, sector_size }
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        debug_assert!(
            buf.len() % self.sector_size == 0,
            "write length {} not aligned to sector size {}",
            buf.len(),
            self.sector_size,
        );
        use std::io::Write;
        self.file.write_all(buf).map_err(Into::into)
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.file.seek(pos).map_err(Into::into)
    }

    fn sync(self) -> Result<()> {
        sync_to_device(self.file)
    }
}

fn build_progress_bar(total_bytes: u64) -> ProgressBar {
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::with_template(
            "  {spinner:.cyan}  [{bar:45.green/white}]  {bytes}/{total_bytes}  {bytes_per_sec}  ETA {eta}",
        )
        .unwrap()
        .progress_chars("##-"),
    );
    pb.set_message("Writing...");
    pb
}

#[cfg(windows)]
fn get_sector_size(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageAccessAlignmentProperty,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR,
        STORAGE_PROPERTY_QUERY,
    };
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
        _ => return 512,
    };

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };

    let mut desc = STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR::default();
    let mut bytes_returned = 0u32;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut desc as *mut _ as *mut _),
            std::mem::size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    if ok.is_ok() && desc.BytesPerPhysicalSector > 0 {
        desc.BytesPerPhysicalSector as usize
    } else {
        512
    }
}

// O_DIRECT needs logical sector alignment (F_NOCACHE needs none), copy mode already queries it
#[cfg(not(windows))]
fn get_sector_size(path: &Path) -> usize {
    crate::copy::logical_sector_size(path) as usize
}

#[cfg(unix)]
fn sync_to_device(file: File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { unix_fdatasync(fd) };
    if ret != 0 {
        warn!("fdatasync returned non-zero ({}); data may not be fully flushed", ret);
    }
    Ok(())
}

#[cfg(unix)]
extern "C" {
    #[link_name = "fdatasync"]
    fn unix_fdatasync(fd: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(windows)]
fn sync_to_device(file: File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::FlushFileBuffers;

    let handle = HANDLE(file.as_raw_handle() as isize);
    let ok = unsafe { FlushFileBuffers(handle) };
    drop(file);
    if ok.is_err() {
        warn!("FlushFileBuffers failed; data may not be fully written to device");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_to_device(_file: File) -> Result<()> {
    warn!("Platform sync not implemented, data flush not guaranteed");
    Ok(())
}
