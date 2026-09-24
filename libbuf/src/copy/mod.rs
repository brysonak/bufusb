/*
    buf - Tool for flashing USB drives across platforms
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
use fatfs::{Dir, FatType, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::list::human_bytes;

#[cfg(any(target_os = "linux", windows))]
mod ntfs;

const FAT32_MAX_FILE: u64 = u32::MAX as u64; // 4 GiB - 1
const ISO_SECTOR: u64 = 2048; // https://wiki.osdev.org/ISO_9660#Sector_size
const COPY_CHUNK: usize = 4 * 1024 * 1024; // big enough that fatfs allocates long cluster runs per call, see stream_copy
const CACHE_BYTES: usize = 32 * 1024 * 1024; // SectorCache's cap, covers a typical stick's whole FAT so it never spills mid write

pub fn run(source: &str, target: &str, dry_run: bool, label: Option<&str>) -> Result<()> {
    let (mut label, explicit_label) = resolve_label(label, Path::new(source));
    let target_path = Path::new(target);
    let sector = logical_sector_size(target_path);
    if !sector.is_power_of_two() || !(512..=4096).contains(&sector) {
        bail!("Fatal: Unsupported logical sector size {} on {}", sector, target);
    }
    info!("Logical sector size: {} bytes", sector);

    let dev_bytes = crate::device_size(target_path)
        .with_context(|| format!("Could not determine size of target {}", target))?;
    if dev_bytes == 0 {
        bail!("Fatal: Could not determine size of target device {}", target);
    }
    let total_sectors = dev_bytes / sector;

    // 1 MiB partition 
    let align_lba = (1024 * 1024) / sector;
    // GPT entry array is 128 entries x 128 bytes = 16 KiB, however many
    // sectors that takes on this device
    let array_sectors = (16384 + sector - 1) / sector;
    let gpt_tail = array_sectors + 1;

    // FAT32 is not valid below 33 MiB (roughly) of clusters, need a comfortable floor
    if total_sectors <= align_lba + gpt_tail + (64 * 1024 * 1024) / sector {
        bail!("Fatal: Target {} is too small for a FAT32 partition", target);
    }
    let part_sectors = total_sectors - align_lba - gpt_tail;

    // Mount the ISO
    let guard = MountGuard::mount(Path::new(source))
        .with_context(|| format!("Failed to mount source ISO {}", source))?;
    let mroot = guard.mount_point().to_path_buf();
    info!("Mounted {} at {}", source, mroot.display());

    let scan = scan_tree(&mroot)?;
    info!(
        "copy: {} across {} files, largest {}, efi_boot={}",
        human_bytes(scan.total_bytes),
        scan.file_count,
        human_bytes(scan.max_file),
        scan.has_efi_boot,
    );

    if scan.max_file > FAT32_MAX_FILE {
        return oversized_file_fallback(source, target, dry_run, &mroot, &scan, &label);
    }

    if label.len() > 11 {
        let short = String::from_utf8_lossy(&fat_label(&label)).trim_end().to_string();
        if explicit_label {
            warn!("Label '{}' truncated to '{}' for FAT32", label, short);
            println!("note: FAT32 labels are 11 characters, using '{}'", short);
        }
        label = short;
    }

    if dry_run {
        println!(
            "\n --dry-run (copy mode): would write a GPT (protective MBR + one {} \
             FAT32 data partition) to {} and copy {} across {} files from {}.",
            human_bytes(part_sectors * sector),
            target,
            human_bytes(scan.total_bytes),
            scan.file_count,
            source,
        );
        if !scan.has_efi_boot {
            println!(
                "note: no /EFI/BOOT/BOOT*.EFI in the image"
            );
        }
        println!();
        return Ok(());
    }

    // Hold the returned guard for the whole write. On windows it keeps the
    // target's volumes locked and dismounted so the raw writes aren't rejected,
    // elsewhere it is useless
    let prep = prepare_target(target_path)?;
    let mut dev = open_target_buffered(target_path)
        .with_context(|| format!("Could not open target {} for writing", target))?;

    info!("Writing GPT: protective MBR + 1 FAT32 data partition, first usable LBA {}", align_lba);

    let mut io = SectorCache::new(&mut dev, sector);

    write_gpt(&mut io, total_sectors, align_lba, sector, &[PartSpec {
        name: &label,
        type_guid: MS_BASIC_DATA,
        sectors: part_sectors,
    }])
    .context("Failed to write GPT")?;

    let pb = build_bar(scan.total_bytes);
    let mut boot_ok = scan.has_efi_boot;
    {
        let mut part = Partition::new(&mut io, align_lba * sector, part_sectors * sector);

        fatfs::format_volume(
            &mut part,
            FormatVolumeOptions::new()
                .fat_type(FatType::Fat32)
                .bytes_per_sector(sector as u16)
                .volume_label(fat_label(&label)),
        )
        .map_err(|e| anyhow::anyhow!("FAT32 format failed: {}", e))?;

        let fs = FileSystem::new(part, FsOptions::new())
            .map_err(|e| anyhow::anyhow!("Could not open new FAT32 filesystem: {}", e))?;

        let skipped = copy_tree(&mroot, &fs, &pb)?;

        if !boot_ok {
            match extract_eltorito_into_fat(Path::new(source), &fs, &pb) {
                Ok(found) => {
                    boot_ok = found;
                    if found {
                        info!("EFI loaders recovered from the El-Torito boot image");
                        println!(
                            "note: EFI bootloader extracted from the ISO's El-Torito boot image"
                        );
                    }
                }
                Err(e) => warn!("El-Torito extraction failed: {:#}", e),
            }
        }

        fs.unmount()
            .map_err(|e| anyhow::anyhow!("FAT32 flush/unmount failed: {}", e))?;

        if skipped > 0 {
            println!("note: {} file(s) could not be read from the ISO and were skipped", skipped);
        }
    }
    io.flush().ok();
    // release the cache's borrow of dev before we touch dev directly
    drop(io);
    pb.finish_with_message("Copied");
    println!("Syncing to device, all data is still being written, do not unplug...");
    dev.sync_data().context("Final sync to device failed")?;
    drop(prep);
    reread_partitions(&dev, target_path);
    drop(dev);

    // Guard drops here too, but drop explicitly so any unmount error logs now
    drop(guard);

    if !boot_ok {
        warn!("No /EFI/BOOT/BOOT*.EFI found in image; result may not be UEFI-bootable");
        println!(
            "warning: no EFI bootloader (/EFI/BOOT/BOOT*.EFI) found in the image \
             or its El-Torito boot image; it may not boot under UEFI"
        );
    }

    println!(
        "\n Copy complete: {} across {} files written to a new FAT32 partition on {}\n",
        human_bytes(scan.total_bytes),
        scan.file_count,
        target,
    );
    Ok(())
}

struct Scan {
    total_bytes: u64,
    max_file: u64,
    file_count: u64,
    has_efi_boot: bool,
}


fn oversized_file_fallback(
    source: &str,
    target: &str,
    dry_run: bool,
    mroot: &Path,
    scan: &Scan,
    label: &str,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = (source, target, dry_run, mroot, scan, label);
        // Due to Microshit, any disk image dealing with NTFS on mac will fail because A): I couldn't find a library that lets me write to NTFS from rust
        // and B): The driver that apple has built-in is read-only. Linux has mkfs.ntfs and windows has Format-Volume with powershell, sorry... I'll make something for the future to fix this. 
        bail!(
            "Fatal: ISO contains a file larger than 4 GiB ({}). FAT32 cannot store it, and \
             macOS has no native NTFS write support",
            human_bytes(scan.max_file)
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    {
        info!(
            "Largest file {} exceeds the FAT32 4 GiB-1 cap, falling back to NTFS + UEFI:NTFS",
            human_bytes(scan.max_file)
        );
        return ntfs::run(source, target, dry_run, label, mroot, scan);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (source, target, dry_run, mroot, label);
        bail!(
            "Fatal: ISO contains a file larger than 4 GiB ({}). FAT32 cannot store it, and \
             the NTFS fallback is only implemented for Linux and Windows.",
            human_bytes(scan.max_file)
        );
    }
}

fn scan_tree(root: &Path) -> Result<Scan> {
    let mut s = Scan {
        total_bytes: 0,
        max_file: 0,
        file_count: 0,
        has_efi_boot: false,
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("Cannot read mounted dir {}", dir.display()))?
        {
            let entry = entry?;
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            // file, or symlink that resolves to a file
            let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            s.total_bytes += len;
            s.file_count += 1;
            if len > s.max_file {
                s.max_file = len;
            }
            if !s.has_efi_boot {
                if let Ok(rel) = p.strip_prefix(root) {
                    if is_efi_boot_loader(rel) {
                        s.has_efi_boot = true;
                    }
                }
            }
        }
    }
    Ok(s)
}

// /EFI/BOOT/BOOT{X64,IA32,AA64,ARM}.EFI, matched non case-sensitive
fn is_efi_boot_loader(rel: &Path) -> bool {
    let comps: Vec<String> = rel
        .iter()
        .map(|c| c.to_string_lossy().into_owned())
        .collect();
    is_efi_boot_comps(&comps)
}

fn is_efi_boot_comps(comps: &[String]) -> bool {
    if comps.len() != 3
        || !comps[0].eq_ignore_ascii_case("efi")
        || !comps[1].eq_ignore_ascii_case("boot")
    {
        return false;
    }
    let f = comps[2].to_ascii_lowercase();
    f.starts_with("boot") && f.ends_with(".efi")
}

fn stream_copy<R: Read + ?Sized, W: Write + ?Sized>(
    src: &mut R,
    dst: &mut W,
    buf: &mut [u8],
    pb: &ProgressBar,
) -> io::Result<u64> {
    let mut total = 0u64;
    loop {
        let n = match src.read(buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        dst.write_all(&buf[..n])?;
        pb.inc(n as u64);
        total += n as u64;
    }
}

fn copy_tree<T: ReadWriteSeek>(mroot: &Path, fs: &FileSystem<T>, pb: &ProgressBar) -> Result<u64> {
    let mut skipped = 0u64;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut stack = vec![mroot.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let rel = p.strip_prefix(mroot).unwrap();
            let comps: Vec<String> =
                rel.iter().map(|c| c.to_string_lossy().into_owned()).collect();
            if comps.is_empty() {
                continue;
            }

            // skip symlinked dirs
            if ft.is_dir() {
                dir_for(fs, &comps)?; // creates the directory (and any parents)
                stack.push(p);
                continue;
            }
            if ft.is_symlink() && p.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                warn!("skipping symlinked directory {}", rel.display());
                skipped += 1;
                continue;
            }

            // regular file (or symlink to one)
            let parent = dir_for(fs, &comps[..comps.len() - 1])?;
            let name = &comps[comps.len() - 1];
            let mut dst = parent
                .create_file(name)
                .map_err(|e| anyhow::anyhow!("FAT create_file {}: {}", rel.display(), e))?;

            let src = File::open(&p).or_else(|first_err| {
                resolve_symlink(mroot, &p)
                    .and_then(|real| File::open(&real).ok())
                    .ok_or(first_err)
            });

            match src {
                Ok(mut src) => {
                    let n = stream_copy(&mut src, &mut dst, &mut buf, pb)
                        .map_err(|e| anyhow::anyhow!("copy {}: {}", rel.display(), e))?;
                    debug!("copied {} ({} bytes)", rel.display(), n);
                }
                Err(e) => {
                    // Truly dangling... Skip
                    warn!("skipping {} ({})", p.display(), e);
                    skipped += 1;
                }
            }
        }
    }
    Ok(skipped)
}

// Resolve (creating as needed) the FAT directory for a path given as
// components. Each returned Dir borrows the filesystem, not its parent, so we
// can walk component by component and reassign.
fn dir_for<'a, T: ReadWriteSeek>(fs: &'a FileSystem<T>, comps: &[String]) -> Result<Dir<'a, T>> {
    let mut cur = fs.root_dir();
    for c in comps {
        cur = match cur.open_dir(c) {
            Ok(d) => d,
            Err(_) => cur
                .create_dir(c)
                .map_err(|e| anyhow::anyhow!("FAT create_dir {}: {}", c, e))?,
        };
    }
    Ok(cur)
}

// Resolve a symlink found on the mounted ISO to a real file path within the mount
fn resolve_symlink(mroot: &Path, link: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(link).ok()?;
    let joined = if target.is_absolute() {
        // strip the leading '/' and reroot under the mount
        let rel = target.strip_prefix("/").unwrap_or(&target);
        mroot.join(rel)
    } else {
        link.parent()?.join(target)
    };
    // canonicalize collapses any `..`
    let real = std::fs::canonicalize(&joined).ok()?;
    let root = std::fs::canonicalize(mroot).ok()?;
    real.starts_with(&root).then_some(real)
}

fn eltorito_efi_rba(cat: &[u8]) -> Option<u64> {
    // header id 0x01, key bytes 0x55 0xAA
    if cat.len() < 2048 || cat[0] != 0x01 || cat[0x1E] != 0x55 || cat[0x1F] != 0xAA {
        return None;
    }
    // 0x88 marks a bootable entry
    let entry_at = |off: usize| -> Option<u64> {
        (cat[off] == 0x88)
            .then(|| u32::from_le_bytes(cat[off + 8..off + 12].try_into().unwrap()) as u64)
    };
    // the initial/default entry at 0x20 is it
    if cat[1] == 0xEF {
        return entry_at(0x20);
    }
    // otherwise walk section headers looking for the EFI platform id, taking that section's first entry
    let mut off = 0x40;
    while off + 0x20 <= 2048 {
        match cat[off] {
            0x90 | 0x91 => {
                let count = u16::from_le_bytes(cat[off + 2..off + 4].try_into().unwrap()) as usize;
                if cat[off + 1] == 0xEF && count >= 1 {
                    return entry_at(off + 0x20);
                }
                if cat[off] == 0x91 {
                    return None;
                }
                off += 0x20 * (1 + count);
            }
            _ => return None,
        }
    }
    None
}

// Find the embedded EFI boot image via the ISO's el-torito boot record.
// Returns its byte offset into the ISO, or None if the ISO has no catalog or
// no EFI entry
fn eltorito_efi_image(iso: &mut File) -> Result<Option<u64>> {
    let mut brvd = [0u8; 2048];
    if iso.seek(SeekFrom::Start(17 * ISO_SECTOR)).is_err() || iso.read_exact(&mut brvd).is_err() {
        return Ok(None);
    }
    if brvd[0] != 0
        || &brvd[1..6] != b"CD001"
        || !brvd[7..].starts_with(b"EL TORITO SPECIFICATION")
    {
        return Ok(None);
    }
    let cat_lba = u32::from_le_bytes(brvd[0x47..0x4B].try_into().unwrap()) as u64;
    let mut cat = [0u8; 2048];
    iso.seek(SeekFrom::Start(cat_lba * ISO_SECTOR))?;
    iso.read_exact(&mut cat)?;
    Ok(eltorito_efi_rba(&cat).map(|rba| rba * ISO_SECTOR))
}

fn walk_fat<T: ReadWriteSeek, F: FnMut(&[String], &mut fatfs::File<'_, T>, u64) -> Result<()>>(
    dir: &Dir<'_, T>,
    prefix: &mut Vec<String>,
    sink: &mut F,
) -> Result<()> {
    for entry in dir.iter() {
        let entry = entry.map_err(|e| anyhow::anyhow!("embedded FAT dir read: {}", e))?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        prefix.push(name);
        if entry.is_dir() {
            let sub = entry.to_dir();
            walk_fat(&sub, prefix, sink)?;
        } else {
            let len = entry.len();
            sink(prefix, &mut entry.to_file(), len)?;
        }
        prefix.pop();
    }
    Ok(())
}

// Extract the El-Torito EFI boot image's contents into the target FAT32
// filesystem root. Returns whether a /EFI/BOOT/BOOT*.EFI landed
fn extract_eltorito_into_fat<T: ReadWriteSeek>(
    source: &Path,
    fs: &FileSystem<T>,
    pb: &ProgressBar,
) -> Result<bool> {
    let mut iso = File::open(source)?;
    let offset = match eltorito_efi_image(&mut iso)? {
        Some(o) => o,
        None => return Ok(false),
    };
    info!("Extracting El-Torito EFI boot image at byte offset {}", offset);

    let len = iso.metadata()?.len().saturating_sub(offset);
    let part = Partition::new(&mut iso, offset, len);
    let img = FileSystem::new(part, FsOptions::new()).map_err(|e| {
        anyhow::anyhow!("embedded EFI image is not a readable FAT filesystem: {}", e)
    })?;

    let mut found_boot = false;
    let mut buf = vec![0u8; COPY_CHUNK];
    walk_fat(&img.root_dir(), &mut Vec::new(), &mut |comps, src, len| {
        let parent = dir_for(fs, &comps[..comps.len() - 1])?;
        let mut dst = parent
            .create_file(&comps[comps.len() - 1])
            .map_err(|e| anyhow::anyhow!("FAT create_file {}: {}", comps.join("/"), e))?;
        dst.truncate()
            .map_err(|e| anyhow::anyhow!("FAT truncate {}: {}", comps.join("/"), e))?;
        pb.inc_length(len);
        stream_copy(src, &mut dst, &mut buf, pb)
            .map_err(|e| anyhow::anyhow!("extract {}: {}", comps.join("/"), e))?;
        found_boot |= is_efi_boot_comps(comps);
        Ok(())
    })?;
    Ok(found_boot)
}

// Same el-torito fallback for NTFS, writing through the OS mount
#[cfg(any(target_os = "linux", windows))]
fn extract_eltorito_into_dir(source: &Path, dst_root: &Path, pb: &ProgressBar) -> Result<bool> {
    let mut iso = File::open(source)?;
    let offset = match eltorito_efi_image(&mut iso)? {
        Some(o) => o,
        None => return Ok(false),
    };
    info!("Extracting El-Torito EFI boot image at byte offset {}", offset);

    let len = iso.metadata()?.len().saturating_sub(offset);
    let part = Partition::new(&mut iso, offset, len);
    let img = FileSystem::new(part, FsOptions::new()).map_err(|e| {
        anyhow::anyhow!("embedded EFI image is not a readable FAT filesystem: {}", e)
    })?;

    let mut found_boot = false;
    let mut buf = vec![0u8; COPY_CHUNK];
    walk_fat(&img.root_dir(), &mut Vec::new(), &mut |comps, src, len| {
        let mut dst_path = dst_root.to_path_buf();
        for c in comps {
            dst_path.push(c);
        }
        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut dst = File::create(&dst_path)?;
        pb.inc_length(len);
        stream_copy(src, &mut dst, &mut buf, pb)
            .map_err(|e| anyhow::anyhow!("extract {}: {}", comps.join("/"), e))?;
        found_boot |= is_efi_boot_comps(comps);
        Ok(())
    })?;
    Ok(found_boot)
}

fn build_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "  {spinner:.cyan}  [{bar:45.green/white}]  {bytes}/{total_bytes}  {bytes_per_sec}  ETA {eta}",
        )
        .unwrap()
        .progress_chars("##-"),
    );
    pb.set_message("Copying...");
    pb
}

// The FAT32 ESP type GUID 
const ESP_TYPE: [u8; 16] = guid_le([
    0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
]);

// MS basic data GUID (GPT byte order), used instead of ESP so desktops don't hide the partition, firmware still boots FAT fine either way
const MS_BASIC_DATA: [u8; 16] = guid_le([
    0xEB, 0xD0, 0xA0, 0xA2, 0xB9, 0xE5, 0x44, 0x33, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
]);


struct PartSpec<'a> {
    name: &'a str,
    type_guid: [u8; 16],
    sectors: u64,
}

fn resolve_label(explicit: Option<&str>, source: &Path) -> (String, bool) {
    let was_explicit = explicit.is_some();
    let picked = explicit
        .map(|s| s.to_string())
        .or_else(|| iso_volume_label(source))
        .unwrap_or_default();
    let label = match sanitize_label(&picked) {
        s if s.is_empty() => "BUF".to_string(),
        s => s,
    };
    (label, was_explicit)
}

// The label reaches mkfs.ntfs argv and a single-quoted powershell string, so
// anything outside this set is replaced
fn sanitize_label(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

// FAT32's BPB label is exactly 11 bytes
fn fat_label(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    for (slot, b) in out.iter_mut().zip(label.bytes()) {
        *slot = b.to_ascii_uppercase();
    }
    out
}

fn iso_volume_label(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut pvd = [0u8; 0x48];
    f.seek(SeekFrom::Start(16 * ISO_SECTOR)).ok()?;
    f.read_exact(&mut pvd).ok()?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let s = pvd[0x28..0x48]
        .iter()
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '_' })
        .collect::<String>()
        .trim()
        .to_string();
    (!s.is_empty()).then_some(s)
}

// writes protective MBR plus primary/backup GPT, sector-size aware for 4Kn, returns (start_lba, end_lba) per partition
fn write_gpt<D: Write + Seek>(
    dev: &mut D,
    total_sectors: u64,
    first_lba: u64,
    sector: u64,
    parts: &[PartSpec<'_>],
) -> Result<Vec<(u64, u64)>> {
    const ENTRY_SIZE: u32 = 128;
    const ENTRY_COUNT: u32 = 128;
    // 16 KiB of entries, the GPT minimum, occupying however many sectors that
    // takes on this device (32 at 512B, 4 at 4KiB)
    let array_sectors = ((ENTRY_SIZE * ENTRY_COUNT) as u64 + sector - 1) / sector;

    if parts.is_empty() || parts.len() > ENTRY_COUNT as usize {
        bail!(
            "Fatal: write_gpt: {} partitions requested, expected 1..={}",
            parts.len(),
            ENTRY_COUNT
        );
    }

    let last_lba = total_sectors - 1;
    let primary_hdr_lba = 1u64;
    let primary_arr_lba = 2u64;
    let backup_hdr_lba = last_lba;
    let backup_arr_lba = last_lba - array_sectors;
    let first_usable = primary_arr_lba + array_sectors;
    let last_usable = backup_arr_lba - 1;

    // Lay partitions out sequentially, back to back, starting at first_lba
    let mut placements = Vec::with_capacity(parts.len());
    let mut cursor = first_lba;
    for spec in parts {
        let end = cursor + spec.sectors - 1;
        placements.push((cursor, end));
        cursor = end + 1;
    }
    let (first_start, _) = placements[0];
    let (_, last_end) = placements[placements.len() - 1];
    if first_start < first_usable || last_end > last_usable {
        bail!(
            "Fatal: Partition layout [{}, {}] does not fit inside usable GPT range [{}, {}]",
            first_start,
            last_end,
            first_usable,
            last_usable
        );
    }

    // partition entry array 
    let mut array = vec![0u8; (ENTRY_SIZE * ENTRY_COUNT) as usize];
    for (i, (spec, (start, end))) in parts.iter().zip(placements.iter()).enumerate() {
        let e = &mut array[i * 128..i * 128 + 128];
        e[0..16].copy_from_slice(&spec.type_guid);
        e[16..32].copy_from_slice(&random_guid()); // unique partition GUID
        e[32..40].copy_from_slice(&start.to_le_bytes());
        e[40..48].copy_from_slice(&end.to_le_bytes());
        for (j, c) in spec.name.encode_utf16().take(36).enumerate() {
            let off = 56 + j * 2;
            e[off..off + 2].copy_from_slice(&c.to_le_bytes());
        }
    }
    let array_crc = crc32(&array);
    let disk_guid = random_guid();

    // GPT header builder, parameterised by which copy we're writing
    let build_header = |my_lba: u64, alt_lba: u64, arr_lba: u64| -> [u8; 92] {
        let mut h = [0u8; 92];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); 
        h[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&arr_lba.to_le_bytes());
        h[80..84].copy_from_slice(&ENTRY_COUNT.to_le_bytes());
        h[84..88].copy_from_slice(&ENTRY_SIZE.to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let hc = crc32(&h); // CRC is computed with the CRC field zeroed
        h[16..20].copy_from_slice(&hc.to_le_bytes());
        h
    };

    let primary = build_header(primary_hdr_lba, backup_hdr_lba, primary_arr_lba);
    let backup = build_header(backup_hdr_lba, primary_hdr_lba, backup_arr_lba);

    // the MBR structure occupies the first 512 bytes of LBA0 regardless of sector size
    let mut pmbr = vec![0u8; sector as usize];
    let p = &mut pmbr[446..462];
    p[0] = 0x00; // not bootable
    p[1] = 0x00;
    p[2] = 0x02;
    p[3] = 0x00; // CHS start 0,2,0
    p[4] = 0xEE; // GPT protective
    p[5] = 0xFF;
    p[6] = 0xFF;
    p[7] = 0xFF; // CHS end = max
    p[8..12].copy_from_slice(&1u32.to_le_bytes()); // starting LBA = 1
    let span = u32::try_from(total_sectors - 1).unwrap_or(u32::MAX);
    p[12..16].copy_from_slice(&span.to_le_bytes());
    pmbr[510] = 0x55;
    pmbr[511] = 0xAA;

    // lay it all down
    gpt_write_at(dev, 0, &pmbr, sector)?;
    gpt_write_at(dev, primary_hdr_lba, &pad_sector(&primary, sector), sector)?;
    gpt_write_at(dev, primary_arr_lba, &array, sector)?;
    gpt_write_at(dev, backup_arr_lba, &array, sector)?;
    gpt_write_at(dev, backup_hdr_lba, &pad_sector(&backup, sector), sector)?;
    dev.flush()?;
    Ok(placements)
}

fn gpt_write_at<D: Write + Seek>(dev: &mut D, lba: u64, bytes: &[u8], sector: u64) -> Result<()> {
    dev.seek(SeekFrom::Start(lba * sector))?;
    dev.write_all(bytes)?;
    Ok(())
}

// pad a GPT header (92 bytes) out to a full sector of zeros
fn pad_sector(header: &[u8], sector: u64) -> Vec<u8> {
    let mut s = vec![0u8; sector as usize];
    s[..header.len()].copy_from_slice(header);
    s
}

// GPT GUIDs mix endianness, first 3 fields LE, last 2 BE, rearranges our RFC order input to on-disk order
const fn guid_le(bytes: [u8; 16]) -> [u8; 16] {
    [
        bytes[3], bytes[2], bytes[1], bytes[0], // data1 LE
        bytes[5], bytes[4], // data2 LE
        bytes[7], bytes[6], // data3 LE
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]
}

// RandomState's OS-seeded hasher is unpredictable enough for disk GUIDs
// Probably switch to CSPRNG in the future...
fn random_guid() -> [u8; 16] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let a = RandomState::new().build_hasher().finish();
    let b = RandomState::new().build_hasher().finish();
    let mut g = [0u8; 16];
    g[..8].copy_from_slice(&a.to_le_bytes());
    g[8..].copy_from_slice(&b.to_le_bytes());
    // stamp RFC4122 version 4 / variant bits so it's a well-formed random GUID
    g[7] = (g[7] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    g
}

// CRC-32 for GPT header/array checksums, bytewise since arrays are tiny
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// bounded view over a sub-range of the inner IO so fatfs sees it as a standalone image
struct Partition<'a, D: Read + Write + Seek> {
    dev: &'a mut D,
    base: u64,
    len: u64,
    pos: u64,
}

impl<'a, D: Read + Write + Seek> Partition<'a, D> {
    fn new(dev: &'a mut D, base: u64, len: u64) -> Self {
        Self { dev, base, len, pos: 0 }
    }
}

impl<D: Read + Write + Seek> Read for Partition<'_, D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let max = std::cmp::min(buf.len() as u64, self.len - self.pos) as usize;
        self.dev.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.dev.read(&mut buf[..max])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<D: Read + Write + Seek> Write for Partition<'_, D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past partition end",
            ));
        }
        let max = std::cmp::min(buf.len() as u64, self.len - self.pos) as usize;
        self.dev.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.dev.write(&buf[..max])?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.dev.flush()
    }
}

impl<D: Read + Write + Seek> Seek for Partition<'_, D> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let np: i64 = match pos {
            SeekFrom::Start(x) => x as i64,
            SeekFrom::End(x) => self.len as i64 + x,
            SeekFrom::Current(x) => self.pos as i64 + x,
        };
        if np < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before partition start",
            ));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

struct CachedSector {
    data: Box<[u8]>,
    dirty: bool,
}

struct SectorCache<'a> {
    dev: &'a mut File,
    sector: usize,
    cap: usize, // max cached sectors before a full flush
    map: HashMap<u64, CachedSector>,
    pos: u64, // logical byte position the layers above see
}

impl<'a> SectorCache<'a> {
    fn new(dev: &'a mut File, sector: u64) -> Self {
        let sector = sector as usize;
        Self {
            dev,
            sector,
            cap: CACHE_BYTES / sector,
            map: HashMap::new(),
            pos: 0,
        }
    }

    // Get the sector holding `lba` into the cache, reading it from the device
    // unless already held
    fn load(&mut self, lba: u64) -> io::Result<&mut CachedSector> {
        if !self.map.contains_key(&lba) {
            if self.map.len() >= self.cap {
                self.flush_dirty()?;
            }
            let mut data = vec![0u8; self.sector].into_boxed_slice();
            self.dev.seek(SeekFrom::Start(lba * self.sector as u64))?;
            let mut filled = 0;
            while filled < self.sector {
                match self.dev.read(&mut data[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            self.map.insert(lba, CachedSector { data, dirty: false });
        }
        Ok(self.map.get_mut(&lba).unwrap())
    }

    fn overwrite(&mut self, lba: u64, bytes: &[u8]) -> io::Result<()> {
        debug_assert_eq!(bytes.len(), self.sector);
        if !self.map.contains_key(&lba) && self.map.len() >= self.cap {
            self.flush_dirty()?;
        }
        match self.map.get_mut(&lba) {
            Some(s) => {
                s.data.copy_from_slice(bytes);
                s.dirty = true;
            }
            None => {
                self.map.insert(
                    lba,
                    CachedSector {
                        data: bytes.to_vec().into_boxed_slice(),
                        dirty: true,
                    },
                );
            }
        }
        Ok(())
    }

    // Write every dirty sector out
    fn flush_dirty(&mut self) -> io::Result<()> {
        let mut lbas: Vec<u64> = self
            .map
            .iter()
            .filter(|(_, s)| s.dirty)
            .map(|(&l, _)| l)
            .collect();
        lbas.sort_unstable();
        let mut run: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < lbas.len() {
            let start = lbas[i];
            run.clear();
            run.extend_from_slice(&self.map[&lbas[i]].data);
            let mut j = i + 1;
            while j < lbas.len() && lbas[j] == lbas[j - 1] + 1 {
                run.extend_from_slice(&self.map[&lbas[j]].data);
                j += 1;
            }
            self.dev.seek(SeekFrom::Start(start * self.sector as u64))?;
            self.dev.write_all(&run)?;
            i = j;
        }
        self.map.clear();
        Ok(())
    }
}

impl Read for SectorCache<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let s = self.sector as u64;
        let lba = self.pos / s;
        let inner = (self.pos % s) as usize;
        let n = std::cmp::min(out.len(), self.sector - inner);
        let sec = self.load(lba)?;
        out[..n].copy_from_slice(&sec.data[inner..inner + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for SectorCache<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let s = self.sector as u64;
        let lba = self.pos / s;
        let inner = (self.pos % s) as usize;

        if inner == 0 && data.len() >= self.sector {
            self.overwrite(lba, &data[..self.sector])?;
            self.pos += self.sector as u64;
            return Ok(self.sector);
        }

        // read-modify-write within the cache
        let n = std::cmp::min(data.len(), self.sector - inner);
        let sec = self.load(lba)?;
        sec.data[inner..inner + n].copy_from_slice(&data[..n]);
        sec.dirty = true;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_dirty()?;
        self.dev.flush()
    }
}

impl Seek for SectorCache<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // End is unused, partition resolves End against its own length and
        // only ever seeks the inner device with absolute Start offsets
        let np: i64 = match pos {
            SeekFrom::Start(x) => x as i64,
            SeekFrom::Current(x) => self.pos as i64 + x,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SectorCache does not support SeekFrom::End",
                ))
            }
        };
        if np < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

impl Drop for SectorCache<'_> {
    fn drop(&mut self) {
        // safety net only, the write paths flush explicitly before dropping
        if self.map.values().any(|s| s.dirty) && self.flush_dirty().is_err() {
            warn!("SectorCache: flush on drop failed, device may be missing writes");
        }
    }
}

#[cfg(target_os = "linux")]
fn logical_sector_size(path: &Path) -> u64 {
    use std::os::unix::io::AsRawFd;
    // BLKSSZGET returns the logical sector size
    const BLKSSZGET: u64 = 0x1268;
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 512,
    };
    let mut size: std::os::raw::c_int = 0;
    let ret = unsafe { ss_ioctl(f.as_raw_fd(), BLKSSZGET, &mut size as *mut _) };
    if ret == 0 && size > 0 {
        size as u64
    } else {
        512
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "ioctl"]
    fn ss_ioctl(fd: std::os::raw::c_int, request: u64, ...) -> std::os::raw::c_int;
}

#[cfg(target_os = "macos")]
fn logical_sector_size(path: &Path) -> u64 {
    use std::os::unix::io::AsRawFd;
    const DKIOCGETBLOCKSIZE: u64 = 0x40046418;
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 512,
    };
    let mut size: u32 = 0;
    let ret = unsafe { ss_ioctl_mac(f.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut size as *mut _) };
    if ret == 0 && size > 0 {
        size as u64
    } else {
        512
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    #[link_name = "ioctl"]
    fn ss_ioctl_mac(fd: std::os::raw::c_int, request: u64, ...) -> std::os::raw::c_int;
}

#[cfg(windows)]
fn logical_sector_size(path: &Path) -> u64 {
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
        _ => return 512,
    };
    let mut geom = DISK_GEOMETRY_EX::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            None,
            0,
            Some(&mut geom as *mut _ as *mut _),
            std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
            Some(&mut returned),
            None,
        )
    };
    let bps = geom.Geometry.BytesPerSector as u64;
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
    if ok.is_ok() && bps >= 512 && bps.is_power_of_two() {
        bps
    } else {
        512
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn logical_sector_size(_path: &Path) -> u64 {
    512
}


pub struct MountGuard {
    mount_point: PathBuf,
    #[allow(dead_code)]
    source: PathBuf,
}

impl MountGuard {
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }
}

#[cfg(target_os = "linux")]
impl MountGuard {
    fn mount(source: &Path) -> Result<Self> {
        use std::process::Command;
        let mp = unique_mountpoint();
        std::fs::create_dir_all(&mp)?;
        let st = Command::new("mount")
            .args(["-o", "loop,ro"])
            .arg(source)
            .arg(&mp)
            .status()
            .context("failed to run mount")?;
        if !st.success() {
            let _ = std::fs::remove_dir(&mp);
            bail!("Fatal: mount exited with {}", st);
        }
        Ok(Self { mount_point: mp, source: source.to_path_buf() })
    }
}

#[cfg(target_os = "macos")]
impl MountGuard {
    fn mount(source: &Path) -> Result<Self> {
        use std::process::Command;
        let mp = unique_mountpoint();
        std::fs::create_dir_all(&mp)?;
        let st = Command::new("hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
            .arg(&mp)
            .arg(source)
            .status()
            .context("failed to run hdiutil attach")?;
        if !st.success() {
            let _ = std::fs::remove_dir(&mp);
            bail!("Fatal: hdiutil attach exited with {}", st);
        }
        Ok(Self { mount_point: mp, source: source.to_path_buf() })
    }
}

#[cfg(windows)]
impl MountGuard {
    fn mount(source: &Path) -> Result<Self> {
        use std::process::Command;
        let ps = format!(
            "$ErrorActionPreference='Stop'; \
             $v = (Mount-DiskImage -ImagePath '{}' -PassThru | Get-Volume).DriveLetter; \
             Write-Output $v",
            source.display()
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .output()
            .context("failed to run Mount-DiskImage")?;
        if !out.status.success() {
            bail!(
                "Fatal: Mount-DiskImage failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let letter = String::from_utf8_lossy(&out.stdout)
            .trim()
            .chars()
            .next()
            .ok_or_else(|| anyhow::anyhow!("could not determine mounted drive letter"))?;
        let mp = PathBuf::from(format!("{}:\\", letter));
        Ok(Self { mount_point: mp, source: source.to_path_buf() })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unique_mountpoint() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("bufusb-iso-{}-{}", std::process::id(), n))
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        use std::process::Command;
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("umount").arg(&self.mount_point).status();
            let _ = std::fs::remove_dir(&self.mount_point);
        }
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(&self.mount_point)
                .status();
            let _ = std::fs::remove_dir(&self.mount_point);
        }
        #[cfg(windows)]
        {
            // dismount echoes its DiskImage object to the inherited console otherwise
            let ps = format!(
                "Dismount-DiskImage -ImagePath '{}' | Out-Null",
                self.source.display()
            );
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
                .status();
        }
        let _ = &self.mount_point; 
    }
}


struct PrepGuard {
    #[cfg(windows)]
    locked: Vec<File>,
}

#[cfg(target_os = "linux")]
fn prepare_target(target: &Path) -> Result<PrepGuard> {
    use std::process::Command;
    // unmounts partitions of this device, matches /dev/sdb1 or nvme0n1p2 style suffixes not a bare prefix, a failed unmount is fatal since a new GPT under a live fs corrupts it
    let t = target.to_string_lossy().to_string();
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            let src = line.split_whitespace().next().unwrap_or("");
            if is_partition_of(src, &t) {
                let ok = Command::new("umount")
                    .arg(src)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    bail!(
                        "Fatal: {} is mounted and could not be unmounted (still in use? a file \
                         manager window or open file will hold it). Close whatever is \
                         using it and retry.",
                        src
                    );
                }
            }
        }
    }
    Ok(PrepGuard {})
}

#[cfg(target_os = "linux")]
fn is_partition_of(cand: &str, dev: &str) -> bool {
    match cand.strip_prefix(dev) {
        Some("") => false, // the whole disk itself
        Some(rest) => {
            let rest = rest.strip_prefix('p').unwrap_or(rest);
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(target_os = "macos")]
fn prepare_target(target: &Path) -> Result<PrepGuard> {
    use std::process::Command;
    let ok = Command::new("diskutil")
        .arg("unmountDisk")
        .arg(target)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!(
            "Fatal: diskutil unmountDisk {} failed; a volume on the target is still in use. \
             Close whatever is using it and retry.",
            target.display()
        );
    }
    Ok(PrepGuard {})
}

#[cfg(windows)]
fn prepare_target(target: &Path) -> Result<PrepGuard> {
    // locks and dismounts every volume on this drive, holding handles open so they persist through the writes, 
    // windows rejects raw writes under a mounted volume otherwise, best effort since a failed lock just warns
    let drive_no = match physical_drive_number(target) {
        Some(n) => n,
        None => {
            warn!("Could not parse PhysicalDrive number from {}", target.display());
            return Ok(PrepGuard { locked: Vec::new() });
        }
    };

    let mut locked = Vec::new();
    for vol in enumerate_volumes() {
        if volume_drive_number(&vol) != Some(drive_no) {
            continue;
        }
        match lock_and_dismount(&vol) {
            Some(h) => {
                info!("Locked + dismounted volume {} on drive {}", vol, drive_no);
                locked.push(h);
            }
            None => warn!("Could not lock volume {} on drive {}; continuing", vol, drive_no),
        }
    }
    if locked.is_empty() {
        debug!("No mounted volumes found on drive {}", drive_no);
    }
    Ok(PrepGuard { locked })
}

#[cfg(windows)]
fn physical_drive_number(target: &Path) -> Option<u32> {
    // "\\.\PhysicalDrive3" -> 3
    target
        .to_string_lossy()
        .rsplit("PhysicalDrive")
        .next()?
        .trim()
        .parse()
        .ok()
}

#[cfg(windows)]
fn enumerate_volumes() -> Vec<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, MAX_PATH};
    use windows::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    };

    let mut out = Vec::new();
    let mut buf = [0u16; MAX_PATH as usize];
    unsafe {
        let handle = match FindFirstVolumeW(&mut buf) {
            Ok(h) => h,
            Err(_) => return out,
        };
        loop {
            out.push(pcwstr_to_string(PCWSTR(buf.as_ptr())));
            if FindNextVolumeW(handle, &mut buf).is_err() {
                // FindNextVolumeW sets ERROR_NO_MORE_FILES at the end
                let _ = ERROR_NO_MORE_FILES;
                break;
            }
        }
        let _ = FindVolumeClose(handle);
    }
    out
}

#[cfg(windows)]
fn pcwstr_to_string(p: windows::core::PCWSTR) -> String {
    unsafe {
        let mut len = 0;
        while *p.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p.0, len))
    }
}

#[cfg(windows)]
fn open_volume(vol: &str) -> Option<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let trimmed = vol.trim_end_matches('\\');
    let wide: Vec<u16> = std::ffi::OsStr::new(trimmed)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match handle {
        Ok(h) if h != INVALID_HANDLE_VALUE => Some(unsafe { File::from_raw_handle(h.0 as *mut _) }),
        _ => None,
    }
}

#[cfg(windows)]
fn volume_drive_number(vol: &str) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    let f = open_volume(vol)?;
    let mut dev_num = STORAGE_DEVICE_NUMBER::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            HANDLE(f.as_raw_handle() as isize),
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some(&mut dev_num as *mut _ as *mut _),
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&mut returned),
            None,
        )
    };
    ok.is_ok().then_some(dev_num.DeviceNumber)
}

#[cfg(windows)]
fn lock_and_dismount(vol: &str) -> Option<File> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME};
    use windows::Win32::System::IO::DeviceIoControl;

    let f = open_volume(vol)?;
    let h = HANDLE(f.as_raw_handle() as isize);
    let mut returned = 0u32;

    let locked = unsafe {
        DeviceIoControl(h, FSCTL_LOCK_VOLUME, None, 0, None, 0, Some(&mut returned), None)
    };
    if locked.is_err() {
        return None;
    }
    let _ = unsafe {
        DeviceIoControl(h, FSCTL_DISMOUNT_VOLUME, None, 0, None, 0, Some(&mut returned), None)
    };
    Some(f)
}

#[cfg(unix)]
fn open_target_buffered(path: &Path) -> Result<File> {
    use std::fs::OpenOptions;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "Cannot open {} (are you running as root?)",
                path.display()
            )
        })?)
}

#[cfg(windows)]
fn open_target_buffered(path: &Path) -> Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match handle {
        Ok(h) if h != INVALID_HANDLE_VALUE => Ok(unsafe { File::from_raw_handle(h.0 as *mut _) }),
        _ => bail!("Fatal: CreateFileW failed for {}", path.display()),
    }
}

#[cfg(target_os = "linux")]
fn reread_partitions(dev: &File, _target: &Path) {
    use std::os::unix::io::AsRawFd;
    // Do not trust reddit as a source for anything. Kept getting BLKRRPART errors because this constant was wong
    // FUCK REDDIT
    const BLKRRPART: u64 = 0x125F;
    const EBUSY: i32 = 16;
    // EBUSY usually means udev or a lingering opener still holds the disk for
    // a moment after our writes. it settles quickly, so retry briefly
    for attempt in 0..5 {
        let ret = unsafe { rr_ioctl(dev.as_raw_fd(), BLKRRPART) };
        if ret == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(EBUSY) || attempt == 4 {
            warn!(
                "BLKRRPART failed ({}); something still holds the disk (a mounted \
                 partition or an open handle). Run partprobe or replug the device \
                 to see the new partition table",
                err
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "ioctl"]
    fn rr_ioctl(fd: std::os::raw::c_int, request: u64, ...) -> std::os::raw::c_int;
}

#[cfg(windows)]
fn reread_partitions(dev: &File, _target: &Path) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::IOCTL_DISK_UPDATE_PROPERTIES;
    use windows::Win32::System::IO::DeviceIoControl;

    let handle = HANDLE(dev.as_raw_handle() as isize);
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_UPDATE_PROPERTIES,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
    if ok.is_err() {
        warn!("IOCTL_DISK_UPDATE_PROPERTIES failed; Explorer may not see the new partition");
    }
}

#[cfg(target_os = "macos")]
fn reread_partitions(_dev: &File, _target: &Path) {
    // diskutil re-scans on its own once our handle closes; nothing to do
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn reread_partitions(_dev: &File, _target: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    #[cfg(unix)]
    fn symlink_loop_is_detected_not_followed() {
       // Regression check for self referential symlinks
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir()
            .join(format!("buf-symlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        symlink(&base, base.join("loop")).unwrap(); // points at its own parent

        let entry = std::fs::read_dir(&base)
            .unwrap()
            .find(|e| e.as_ref().unwrap().file_name() == "loop")
            .unwrap()
            .unwrap();
        let ft = entry.file_type().unwrap();
        assert!(ft.is_symlink() && !ft.is_dir(), "sanity: symlink's raw type isn't dir");
        assert!(entry.path().metadata().unwrap().is_dir(), "target is a dir, must be skipped not recursed");

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn gpt_roundtrips_and_validates() {
        for &sector in &[512u64, 4096] {
            gpt_check(sector);
        }
    }

    fn gpt_check(sector: u64) {
        let total_sectors: u64 = (256 * 1024 * 1024) / sector;
        let part_start = (1024 * 1024) / sector;
        let array_sectors = (16384 + sector - 1) / sector;
        let part_sectors = total_sectors - part_start - array_sectors - 1;

        let path = std::env::temp_dir()
            .join(format!("buf-gpt-test-{}-{}.img", std::process::id(), sector));
        {
            let f = File::create(&path).unwrap();
            f.set_len(total_sectors * sector).unwrap();
        }
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        write_gpt(&mut f, total_sectors, part_start, sector, &[PartSpec {
            name: "BUF",
            type_guid: ESP_TYPE,
            sectors: part_sectors,
        }])
        .unwrap();

        let read_lba = |f: &mut File, lba: u64| -> Vec<u8> {
            let mut b = vec![0u8; sector as usize];
            f.seek(SeekFrom::Start(lba * sector)).unwrap();
            f.read_exact(&mut b).unwrap();
            b
        };

        let pmbr = read_lba(&mut f, 0);
        assert_eq!(pmbr[450], 0xEE, "protective MBR partition type");
        assert_eq!([pmbr[510], pmbr[511]], [0x55, 0xAA]);

        let mut check_header = |f: &mut File, hdr_lba: u64, arr_lba: u64, my_expect: u64| {
            let sec = read_lba(f, hdr_lba);
            let h = &sec[..92];
            assert_eq!(&h[0..8], b"EFI PART");
            let saved = u32::from_le_bytes(h[16..20].try_into().unwrap());
            let mut z = h.to_vec();
            z[16..20].copy_from_slice(&[0; 4]);
            assert_eq!(crc32(&z), saved, "header self-CRC at LBA {}", hdr_lba);
            assert_eq!(u64::from_le_bytes(h[24..32].try_into().unwrap()), my_expect);
            let arr_crc = u32::from_le_bytes(h[88..92].try_into().unwrap());
            let mut arr = vec![0u8; 128 * 128];
            f.seek(SeekFrom::Start(arr_lba * sector)).unwrap();
            f.read_exact(&mut arr).unwrap();
            assert_eq!(crc32(&arr), arr_crc, "array CRC at LBA {}", arr_lba);
            assert_eq!(&arr[0..16], &ESP_TYPE);
            assert_eq!(u64::from_le_bytes(arr[32..40].try_into().unwrap()), part_start);
            assert_eq!(
                u64::from_le_bytes(arr[40..48].try_into().unwrap()),
                part_start + part_sectors - 1
            );
        };

        let last = total_sectors - 1;
        check_header(&mut f, 1, 2, 1); // primary
        check_header(&mut f, last, last - array_sectors, last); // backup

        let prim = read_lba(&mut f, 1);
        let back = read_lba(&mut f, last);
        assert_eq!(u64::from_le_bytes(prim[32..40].try_into().unwrap()), last);
        assert_eq!(u64::from_le_bytes(back[32..40].try_into().unwrap()), 1);

        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn label_resolution_and_fat_padding() {
        assert_eq!(&fat_label("arch")[..], b"ARCH       ");
        assert_eq!(&fat_label("ARCH_202608")[..], b"ARCH_202608");
        assert_eq!(&fat_label("way too long a label")[..], b"WAY TOO LON");

        assert_eq!(sanitize_label("Ubuntu 24.04'; rm -rf"), "Ubuntu 24_04__ rm -rf");
        assert_eq!(sanitize_label("  ***  "), "___");

        // a file with no ISO9660 PVD falls through to the default
        let empty = std::env::temp_dir().join(format!("buf-label-test-{}.img", std::process::id()));
        File::create(&empty).unwrap().set_len(1 << 16).unwrap();
        assert_eq!(iso_volume_label(&empty), None);
        assert_eq!(resolve_label(None, &empty), ("BUF".to_string(), false));
        assert_eq!(resolve_label(Some("MYSTICK"), &empty), ("MYSTICK".to_string(), true));

        // now stamp a minimal PVD in and check we read the identifier back
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&empty).unwrap();
            f.seek(SeekFrom::Start(16 * ISO_SECTOR)).unwrap();
            let mut pvd = [b' '; 0x48];
            pvd[0] = 1;
            pvd[1..6].copy_from_slice(b"CD001");
            pvd[0x28..0x28 + 11].copy_from_slice(b"ARCH_202608");
            f.write_all(&pvd).unwrap();
        }
        assert_eq!(iso_volume_label(&empty).as_deref(), Some("ARCH_202608"));
        assert_eq!(resolve_label(None, &empty), ("ARCH_202608".to_string(), false));
        assert_eq!(resolve_label(Some("MYSTICK"), &empty), ("MYSTICK".to_string(), true));

        let _ = std::fs::remove_file(&empty);
    }

    #[test]
    fn esp_type_guid_on_disk_order() {
        let esp = guid_le([
            0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ]);
        assert_eq!(
            esp,
            [
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B
            ]
        );
    }

    #[test]
    fn eltorito_catalog_parsing() {
        // EFI advertised via a section header after a BIOS initial entry
        let mut cat = vec![0u8; 2048];
        cat[0] = 0x01; // validation header id
        cat[1] = 0x00; // platform: x86
        cat[0x1E] = 0x55;
        cat[0x1F] = 0xAA;
        cat[0x20] = 0x88; // BIOS initial entry, should be ignored
        cat[0x28..0x2C].copy_from_slice(&100u32.to_le_bytes());
        cat[0x40] = 0x91; // final section header
        cat[0x41] = 0xEF; // platform: EFI
        cat[0x42..0x44].copy_from_slice(&1u16.to_le_bytes());
        cat[0x60] = 0x88; // its bootable entry
        cat[0x68..0x6C].copy_from_slice(&7777u32.to_le_bytes());
        assert_eq!(eltorito_efi_rba(&cat), Some(7777));

        let mut cat2 = vec![0u8; 2048];
        cat2[0] = 0x01;
        cat2[1] = 0xEF;
        cat2[0x1E] = 0x55;
        cat2[0x1F] = 0xAA;
        cat2[0x20] = 0x88;
        cat2[0x28..0x2C].copy_from_slice(&42u32.to_le_bytes());
        assert_eq!(eltorito_efi_rba(&cat2), Some(42));

        assert_eq!(eltorito_efi_rba(&vec![0u8; 2048]), None);
        cat[0x41] = 0x00; // section platform no longer EFI
        assert_eq!(eltorito_efi_rba(&cat), None);
    }

    #[test]
    fn sector_cache_roundtrips() {
        let sector = 512u64;
        let size = (sector as usize) * 256;
        let path = std::env::temp_dir()
            .join(format!("buf-cache-test-{}.img", std::process::id()));
        {
            let f = File::create(&path).unwrap();
            f.set_len(size as u64).unwrap();
        }
        let mut mirror = vec![0u8; size];

        // deterministic xorshift so failures reproduce
        let mut state: u64 = 0x00C0FFEE;
        let mut next = |m: u64| -> u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % m
        };

        let mut dev = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        {
            let mut c = SectorCache::new(&mut dev, sector);
            for i in 0..600 {
                let off = next(size as u64 - 1);
                let max_len = std::cmp::min(3000, size as u64 - off);
                let len = (1 + next(max_len)) as usize;
                c.seek(SeekFrom::Start(off)).unwrap();
                if i % 3 == 0 {
                    // read back through the cache, must match the mirror
                    let mut got = vec![0u8; len];
                    c.read_exact(&mut got).unwrap();
                    assert_eq!(got, &mirror[off as usize..off as usize + len]);
                } else {
                    let chunk: Vec<u8> = (0..len).map(|_| next(256) as u8).collect();
                    c.write_all(&chunk).unwrap();
                    mirror[off as usize..off as usize + len].copy_from_slice(&chunk);
                }
            }
            c.flush().unwrap();
        }

        let mut disk = vec![0u8; size];
        dev.seek(SeekFrom::Start(0)).unwrap();
        dev.read_exact(&mut disk).unwrap();
        assert_eq!(disk, mirror, "flushed device diverged from mirror");

        drop(dev);
        let _ = std::fs::remove_file(&path);
    }
}
