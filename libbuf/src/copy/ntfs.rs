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
use indicatif::ProgressBar;
use log::{debug, info, warn};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{
    build_bar, gpt_write_at, open_target_buffered, prepare_target, reread_partitions,
    resolve_symlink, write_gpt, PartSpec, Scan, ESP_TYPE, MS_BASIC_DATA,
};
use crate::list::human_bytes;

const SECTOR: u64 = 512;

const LOADER_IMG: &[u8] = include_bytes!("../../assets/uefi-ntfs.img");
const ALIGN_LBA: u64 = 2048; // 1 MiB, matches the plain copy-mode alignment
const GPT_TAIL: u64 = 33;

// Same floor the FAT32 copy path uses for its single partition, there's no
// reason to allow a smaller NTFS data partition than we'd allow a FAT32 one
const MIN_DATA_SECTORS: u64 = 64 * 1024 * 1024 / SECTOR;

pub fn run(source: &str, target: &str, dry_run: bool, label: &str, mroot: &Path, scan: &Scan,) -> Result<()> {
    let target_path = Path::new(target);
    let dev_bytes = crate::device_size(target_path)
        .with_context(|| format!("Could not determine size of target {}", target))?;
    if dev_bytes == 0 {
        bail!("Could not determine size of target device {}", target);
    }
    let total_sectors = dev_bytes / SECTOR;
    let loader_sectors = round_up_sectors(LOADER_IMG.len() as u64, SECTOR);

    if total_sectors <= ALIGN_LBA + GPT_TAIL + loader_sectors + MIN_DATA_SECTORS {
        bail!("Target {} is too small for an NTFS + UEFI:NTFS layout", target);
    }
    let ntfs_sectors = total_sectors - ALIGN_LBA - GPT_TAIL - loader_sectors;

    info!(
        "ntfs-copy: {} across {} files, largest {}, efi_boot={}",
        human_bytes(scan.total_bytes),
        scan.file_count,
        human_bytes(scan.max_file),
        scan.has_efi_boot,
    );

    if dry_run {
        println!(
            "\n --dry-run (copy mode, NTFS fallback): would write a GPT ({} NTFS data \
             partition + {} UEFI:NTFS loader partition) to {} and copy {} across {} files \
             from {}.",
            human_bytes(ntfs_sectors * SECTOR),
            human_bytes(loader_sectors * SECTOR),
            target,
            human_bytes(scan.total_bytes),
            scan.file_count,
            source,
        );
        println!();
        return Ok(());
    }

    let prep = prepare_target(target_path)?;
    let mut dev = open_target_buffered(target_path)
        .with_context(|| format!("Could not open target {} for writing", target))?;

    let logical = super::logical_sector_size(target_path);
    if logical != SECTOR {
        bail!(
            "Fatal: target reports a {}-byte logical sector, the NTFS fallback currently only \
             supports 512-byte-logical devices. Use --mode dd instead.",
            logical
        );
    }

    let mut io = super::SectorCache::new(&mut dev, SECTOR);

    let placements = write_gpt(&mut io, total_sectors, ALIGN_LBA, SECTOR, &[
        PartSpec { name: label, type_guid: MS_BASIC_DATA, sectors: ntfs_sectors },
        PartSpec { name: "UEFI_NTFS", type_guid: ESP_TYPE, sectors: loader_sectors },
    ])
    .context("Failed to write GPT")?;
    let (loader_start, _) = placements[1];

    let mut loader_buf = vec![0u8; (loader_sectors * SECTOR) as usize];
    loader_buf[..LOADER_IMG.len()].copy_from_slice(LOADER_IMG);
    gpt_write_at(&mut io, loader_start, &loader_buf, SECTOR)?;
    io.flush()?;
    drop(io);

    dev.sync_data().context("Sync of GPT + loader to device failed")?;
    drop(prep);
    reread_partitions(&dev, target_path);
    drop(dev);

    let pb = build_bar(scan.total_bytes);
    let (skipped, extracted_boot) =
        format_and_copy(target_path, mroot, Path::new(source), !scan.has_efi_boot, &pb, label)?;
    pb.finish_with_message("Done");

    if skipped > 0 {
        println!("note: {} file(s) could not be read from the ISO and were skipped", skipped);
    }
    if extracted_boot {
        println!("note: EFI bootloader extracted from the ISO's El-Torito boot image");
    }
    if !scan.has_efi_boot && !extracted_boot {
        warn!("No /EFI/BOOT/BOOT*.EFI found in image; result may not be UEFI-bootable (Note: Try flashing with --mode dd)");
        println!(
            "warning: no EFI bootloader (/EFI/BOOT/BOOT*.EFI) found in the image \
             or its El-Torito boot image; it may not boot under UEFI"
        );
    }

    println!(
        "\n Copy complete (NTFS + UEFI:NTFS): {} across {} files written to {}\n",
        human_bytes(scan.total_bytes),
        scan.file_count,
        target,
    );
    Ok(())
}

fn round_up_sectors(bytes: u64, sector: u64) -> u64 {
    (bytes + sector - 1) / sector
}

fn copy_tree_native(mroot: &Path, dst_root: &Path, pb: &ProgressBar) -> Result<u64> {
    let mut skipped = 0u64;
    let mut stack = vec![mroot.to_path_buf()];
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
            let rel = p.strip_prefix(mroot).unwrap();
            if rel.as_os_str().is_empty() {
                continue;
            }
            let dst = dst_root.join(rel);

            // symlink dirs are skipped
            if ft.is_dir() {
                std::fs::create_dir_all(&dst)
                    .map_err(|e| anyhow::anyhow!("mkdir {}: {}", dst.display(), e))?;
                stack.push(p);
                continue;
            }
            if ft.is_symlink() && p.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                warn!("skipping symlinked directory {}", rel.display());
                skipped += 1;
                continue;
            }

            let src = File::open(&p).or_else(|first_err| {
                resolve_symlink(mroot, &p)
                    .and_then(|real| File::open(&real).ok())
                    .ok_or(first_err)
            });

            match src {
                Ok(mut src) => {
                    let mut dst_file = File::create(&dst)
                        .map_err(|e| anyhow::anyhow!("create {}: {}", dst.display(), e))?;
                    let n = std::io::copy(&mut src, &mut dst_file)
                        .map_err(|e| anyhow::anyhow!("copy {}: {}", rel.display(), e))?;
                    pb.inc(n);
                    debug!("copied {} ({} bytes)", rel.display(), n);
                }
                Err(e) => {
                    warn!("skipping {} ({})", p.display(), e);
                    skipped += 1;
                }
            }
        }
    }
    Ok(skipped)
}

// Linux: format partition 1 with mkfs.ntfs, mount it, copy, unmount

#[cfg(target_os = "linux")]
fn format_and_copy(
    target: &Path,
    mroot: &Path,
    source: &Path,
    need_eltorito: bool,
    pb: &ProgressBar,
    label: &str,
) -> Result<(u64, bool)> {
    let part_node = partition_node(target, 1);
    wait_for_node(&part_node)?;

    let st = Command::new("mkfs.ntfs")
        .args(["-f", "-F", "-L", label])
        .arg(&part_node)
        .status()
        .context(
            "failed to run mkfs.ntfs (is ntfs-3g and ntfsprogs installed?)",
        )?;
    if !st.success() {
        bail!("mkfs.ntfs exited with {}", st);
    }

    let mount = NtfsMount::mount(&part_node)?;
    let skipped = copy_tree_native(mroot, mount.mount_point(), pb)?;
    let mut extracted = false;
    if need_eltorito {
        match super::extract_eltorito_into_dir(source, mount.mount_point(), pb) {
            Ok(found) => extracted = found,
            Err(e) => warn!("El-Torito extraction failed: {:#}", e),
        }
    }
    Ok((skipped, extracted))
}

#[cfg(target_os = "linux")]
fn partition_node(disk: &Path, index: u32) -> PathBuf {
    let s = disk.to_string_lossy();
    let sep = if s.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false) { "p" } else { "" };
    PathBuf::from(format!("{}{}{}", s, sep, index))
}

// BLKRRPART is synchronous-ish but udev still needs a moment to create the new device node
#[cfg(target_os = "linux")]
fn wait_for_node(node: &Path) -> Result<()> {
    for _ in 0..40 {
        if node.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(125));
    }
    bail!("Partition node {} did not appear after writing the GPT", node.display());
}

#[cfg(target_os = "linux")]
struct NtfsMount {
    mount_point: PathBuf,
}

#[cfg(target_os = "linux")]
impl NtfsMount {
    fn mount(node: &Path) -> Result<Self> {
        let mp = std::env::temp_dir().join(format!(
            "bufusb-ntfs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&mp)?;

        // Prefer the in-kernel ntfs3 driver, fall back to the ntfs-3g FUSE
        // driver if ntfs3 isn't available on this kernel/distro
        let ok = Command::new("mount")
            .args(["-t", "ntfs3", "-o", "rw"])
            .arg(node)
            .arg(&mp)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let ok = ok
            || Command::new("mount")
                .args(["-t", "ntfs-3g"])
                .arg(node)
                .arg(&mp)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

        if !ok {
            let _ = std::fs::remove_dir(&mp);
            bail!(
                "Fatal: Could not mount {} as NTFS (tried ntfs3 and ntfs-3g). Is one of them installed?",
                node.display()
            );
        }
        Ok(Self { mount_point: mp })
    }

    fn mount_point(&self) -> &Path {
        &self.mount_point
    }
}

#[cfg(target_os = "linux")]
impl Drop for NtfsMount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mount_point).status();
        let _ = std::fs::remove_dir(&self.mount_point);
    }
}

#[cfg(windows)]
fn format_and_copy(
    target: &Path,
    mroot: &Path,
    source: &Path,
    need_eltorito: bool,
    pb: &ProgressBar,
    label: &str,
) -> Result<(u64, bool)> {
    let drive_no = super::physical_drive_number(target)
        .ok_or_else(|| anyhow::anyhow!("Could not parse PhysicalDrive number from {}", target.display()))?;

    // I hate powershell
    let ps = format!(
        "$ErrorActionPreference='Stop'; \
         Update-HostStorageCache; \
         Get-Partition -DiskNumber {n} -PartitionNumber 1 | \
         Format-Volume -FileSystem NTFS -NewFileSystemLabel '{label}' -Confirm:$false | Out-Null; \
         $p = Get-Partition -DiskNumber {n} -PartitionNumber 1; \
         if (-not $p.DriveLetter) {{ \
             $p | Add-PartitionAccessPath -AssignDriveLetter | Out-Null; \
             $p = Get-Partition -DiskNumber {n} -PartitionNumber 1; \
         }}; \
         Write-Output $p.DriveLetter",
        n = drive_no,
        label = label
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .output()
        .context("failed to run Format-Volume")?;
    if !out.status.success() {
        bail!("NTFS format failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let letter = String::from_utf8_lossy(&out.stdout)
        .trim()
        .chars()
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not determine the assigned drive letter"))?;
    let mp = PathBuf::from(format!("{}:\\", letter));

    let skipped = copy_tree_native(mroot, &mp, pb)?;
    let mut extracted = false;
    if need_eltorito {
        match super::extract_eltorito_into_dir(source, &mp, pb) {
            Ok(found) => extracted = found,
            Err(e) => warn!("El-Torito extraction failed: {:#}", e),
        }
    }

    Ok((skipped, extracted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_up_sectors_basic() {
        assert_eq!(round_up_sectors(0, 512), 0);
        assert_eq!(round_up_sectors(1, 512), 1);
        assert_eq!(round_up_sectors(512, 512), 1);
        assert_eq!(round_up_sectors(513, 512), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partition_node_naming() {
        assert_eq!(partition_node(Path::new("/dev/sdb"), 1).to_str().unwrap(), "/dev/sdb1");
        assert_eq!(
            partition_node(Path::new("/dev/nvme0n1"), 2).to_str().unwrap(),
            "/dev/nvme0n1p2"
        );
    }
}
