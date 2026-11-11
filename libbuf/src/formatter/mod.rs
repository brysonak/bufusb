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

mod exfat;
mod fat;
mod os;

use anyhow::{bail, Context, Result};
use log::{info, warn};
use std::fmt;
use std::io::{Seek, Write};
use std::path::Path;

use crate::copy::{
    fat32_cluster, gpt_write_at, guid_le, open_target_buffered, prepare_target, random_guid,
    reread_partitions, sanitize_label, write_gpt, PartSpec, SectorCache, MS_BASIC_DATA,
};
use crate::list::human_bytes;

const LINUX_FS: [u8; 16] = guid_le([
    0x0F, 0xC6, 0x3D, 0xAF, 0x84, 0x83, 0x47, 0x72, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fs {
    Fat32,
    Exfat,
    Ntfs,
    Ext4,
    Udf,
    Refs,
}

impl Fs {
    fn default_table(self) -> Table {
        match self {
            Fs::Fat32 | Fs::Exfat => Table::Mbr,
            _ => Table::Gpt,
        }
    }

    fn mbr_type(self) -> u8 {
        match self {
            Fs::Fat32 => 0x0C, // FAT32 with LBA addressing
            Fs::Ext4 => 0x83,
            Fs::Exfat | Fs::Ntfs | Fs::Udf | Fs::Refs => 0x07,
        }
    }

    fn gpt_type(self) -> [u8; 16] {
        match self {
            Fs::Ext4 => LINUX_FS,
            _ => MS_BASIC_DATA,
        }
    }

    fn max_label(self) -> usize {
        match self {
            Fs::Fat32 | Fs::Exfat => 11,
            Fs::Ext4 => 16,
            Fs::Udf => 30,
            Fs::Ntfs | Fs::Refs => 32,
        }
    }

    fn native(self) -> bool {
        matches!(self, Fs::Fat32 | Fs::Exfat)
    }

    fn available_here(self) -> Result<()> {
        let ok = match self {
            Fs::Fat32 | Fs::Exfat => true,
            Fs::Ntfs => cfg!(any(target_os = "linux", windows)),
            Fs::Ext4 => cfg!(target_os = "linux"),
            Fs::Udf => cfg!(any(target_os = "linux", target_os = "macos", windows)),
            Fs::Refs => cfg!(windows),
        };
        if !ok {
            bail!(
                "Fatal: {} can't be created on {}",
                self,
                std::env::consts::OS
            );
        }
        os::check_tool(self)
    }
}

impl fmt::Display for Fs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Fs::Fat32 => "FAT32",
            Fs::Exfat => "exFAT",
            Fs::Ntfs => "NTFS",
            Fs::Ext4 => "ext4",
            Fs::Udf => "UDF",
            Fs::Refs => "ReFS",
        })
    }
}

impl std::str::FromStr for Fs {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "fat32" | "fat" => Fs::Fat32,
            "exfat" => Fs::Exfat,
            "ntfs" => Fs::Ntfs,
            "ext4" => Fs::Ext4,
            "udf" => Fs::Udf,
            "refs" => Fs::Refs,
            _ => bail!(
                "unknown filesystem '{}', expected fat32, exfat, ntfs, ext4, udf or refs",
                s
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Mbr,
    Gpt,
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Table::Mbr => "MBR",
            Table::Gpt => "GPT",
        })
    }
}

#[derive(Debug)]
pub struct Plan {
    target: String,
    fs: Fs,
    table: Table,
    sector: u64,
    total_sectors: u64,
    part_start: u64,
    part_sectors: u64,
    label: Option<String>,
    cluster: u64,
    dev_bytes: u64,
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}): {} partition table, one {} {} partition",
            self.target,
            human_bytes(self.dev_bytes),
            self.table,
            human_bytes(self.part_sectors * self.sector),
            self.fs,
        )?;
        if self.cluster > 0 {
            write!(f, ", {} clusters", human_bytes(self.cluster))?;
        }
        match &self.label {
            Some(l) => write!(f, ", label '{}'", l),
            None => write!(f, ", no label"),
        }
    }
}

pub fn plan(target: &str, fs: Fs, table: Option<Table>, label: Option<&str>) -> Result<Plan> {
    fs.available_here()?;
    let path = Path::new(target);
    let sector = crate::copy::logical_sector_size(path);
    if !sector.is_power_of_two() || !(512..=4096).contains(&sector) {
        bail!(
            "Fatal: Unsupported logical sector size {} on {}",
            sector,
            target
        );
    }
    let dev_bytes = crate::device_size(path)
        .with_context(|| format!("Could not determine size of target {}", target))?;
    if dev_bytes == 0 {
        bail!(
            "Fatal: Could not determine size of target device {}",
            target
        );
    }
    let total_sectors = dev_bytes / sector;
    let table = table.unwrap_or(fs.default_table());

    let part_start = 1024 * 1024 / sector;
    let tail = 16384 / sector + 1;
    if total_sectors <= part_start + tail + 64 * 1024 * 1024 / sector {
        bail!("Fatal: Target {} is too small to format", target);
    }
    let part_sectors = total_sectors - part_start - tail;
    if table == Table::Mbr && part_start + part_sectors > u32::MAX as u64 {
        bail!(
            "Fatal: {} is too big for an MBR partition table (max {} at {}-byte sectors), use --gpt",
            target,
            human_bytes(u32::MAX as u64 * sector),
            sector
        );
    }

    let mut label = label.map(sanitize_label).filter(|l| !l.is_empty());
    if let Some(l) = label.as_mut().filter(|l| l.len() > fs.max_label()) {
        l.truncate(fs.max_label());
        let short = l.trim_end().to_string();
        warn!(
            "{} labels are {} characters, using '{}'",
            fs,
            fs.max_label(),
            short
        );
        *l = short;
    }
    let part_bytes = part_sectors * sector;
    let cluster = match fs {
        Fs::Fat32 => fat32_cluster(part_bytes, sector),
        Fs::Exfat => exfat::cluster_size(part_bytes).max(sector),
        _ => 0, // the OS tool picks
    };

    let p = Plan {
        target: target.to_string(),
        fs,
        table,
        sector,
        total_sectors,
        part_start,
        part_sectors,
        label,
        cluster,
        dev_bytes,
    };
    info!("fmt plan: {} (sector {} bytes)", p, sector);
    Ok(p)
}

pub fn write(p: &Plan, fake_check: bool) -> Result<()> {
    let path = Path::new(&p.target);
    let prep = prepare_target(path)?;
    if fake_check {
        crate::fakecheck::check(path, 0, p.dev_bytes)?;
    }
    let mut dev = open_target_buffered(path)
        .with_context(|| format!("Could not open target {} for writing", p.target))?;
    let mut io = SectorCache::new(&mut dev, p.sector);

    match p.table {
        Table::Gpt => {
            write_gpt(
                &mut io,
                p.total_sectors,
                p.part_start,
                p.sector,
                &[PartSpec {
                    name: p.label.as_deref().unwrap_or(""),
                    type_guid: p.fs.gpt_type(),
                    sectors: p.part_sectors,
                }],
            )
            .context("Failed to write GPT")?;
        }
        Table::Mbr => write_mbr(&mut io, p).context("Failed to write MBR")?,
    }
    match p.fs {
        Fs::Fat32 => fat::format(&mut io, p)?,
        Fs::Exfat => exfat::format(&mut io, p)?,
        _ => {} // once the table is live, os::format below
    }

    io.flush()
        .context("Flushing cached sectors to the device failed")?;
    drop(io);
    crate::say!("Syncing to device, do not unplug...");
    dev.sync_data().context("Final sync to device failed")?;
    drop(prep);
    let reread = reread_partitions(&dev, path);
    info!("Partition table re-read by the OS: {}", reread);
    drop(dev);
    if !p.fs.native() {
        os::format(p, reread)?;
    }
    crate::say!("\n Formatted {} as {}\n", p.target, p.fs);
    Ok(())
}

fn write_mbr<D: Write + Seek>(dev: &mut D, p: &Plan) -> Result<()> {
    let tail = p.total_sectors - p.part_start - p.part_sectors;
    gpt_write_at(
        dev,
        0,
        &vec![0u8; (p.part_start * p.sector) as usize],
        p.sector,
    )?;
    gpt_write_at(
        dev,
        p.total_sectors - tail,
        &vec![0u8; (tail * p.sector) as usize],
        p.sector,
    )?;

    let mut mbr = vec![0u8; p.sector as usize];
    mbr[440..444].copy_from_slice(&random_guid()[..4]); // disk signature, Windows wants it set
    let last = p.part_start + p.part_sectors - 1;
    let e = &mut mbr[446..462];
    e[1..4].copy_from_slice(&chs(p.part_start));
    e[4] = p.fs.mbr_type();
    e[5..8].copy_from_slice(&chs(last));
    e[8..12].copy_from_slice(&(p.part_start as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(p.part_sectors as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    gpt_write_at(dev, 0, &mbr, p.sector)?;
    dev.flush()?;
    Ok(())
}

fn chs(lba: u64) -> [u8; 3] {
    let c = lba / (255 * 63);
    if c > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let h = (lba / 63) % 255;
    let s = lba % 63 + 1;
    [h as u8, s as u8 | ((c >> 2) as u8 & 0xC0), c as u8]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, SeekFrom};

    #[test]
    fn chs_matches_fdisk() {
        assert_eq!(chs(0), [0x00, 0x01, 0x00]);
        assert_eq!(chs(2048), [0x20, 0x21, 0x00]);
        assert_eq!(chs(1023 * 255 * 63), [0x00, 0xC1, 0xFF]);
        assert_eq!(chs(1024 * 255 * 63), [0xFE, 0xFF, 0xFF]);
    }

    #[test]
    fn fs_names() {
        assert_eq!("FAT32".parse::<Fs>().unwrap(), Fs::Fat32);
        assert_eq!(" fat ".parse::<Fs>().unwrap(), Fs::Fat32);
        assert_eq!("exFAT".parse::<Fs>().unwrap(), Fs::Exfat);
        assert_eq!("ReFS".parse::<Fs>().unwrap(), Fs::Refs);
        assert!("xfs".parse::<Fs>().is_err());
    }

    #[test]
    fn formats_an_image_both_ways() {
        for table in [Table::Mbr, Table::Gpt] {
            let path = std::env::temp_dir().join(format!(
                "buf-fmt-test-{}-{}.img",
                std::process::id(),
                table
            ));
            let size = 128 * 1024 * 1024u64;
            {
                let mut f = File::create(&path).unwrap();
                f.set_len(size).unwrap();
                f.seek(SeekFrom::Start(512)).unwrap();
                f.write_all(b"EFI PART").unwrap();
                f.seek(SeekFrom::Start(size - 512)).unwrap();
                f.write_all(b"EFI PART").unwrap();
            }
            let target = path.to_string_lossy().into_owned();
            let p = plan(&target, Fs::Fat32, Some(table), Some("my stick 2026")).unwrap();
            assert_eq!(p.label.as_deref(), Some("my stick 20"));
            write(&p, false).unwrap();

            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut lba0 = [0u8; 512];
            f.read_exact(&mut lba0).unwrap();
            assert_eq!(lba0[510..], [0x55, 0xAA]);
            assert_eq!(lba0[450], if table == Table::Mbr { 0x0C } else { 0xEE });
            let mut hdr = [0u8; 8];
            f.seek(SeekFrom::Start(size - 512)).unwrap();
            f.read_exact(&mut hdr).unwrap();
            assert_eq!(
                &hdr == b"EFI PART",
                table == Table::Gpt,
                "stale or missing backup GPT"
            );

            let base = 1024 * 1024;
            let mut bpb = [0u8; 512];
            f.seek(SeekFrom::Start(base)).unwrap();
            f.read_exact(&mut bpb).unwrap();
            assert_eq!(
                u32::from_le_bytes(bpb[0x1C..0x20].try_into().unwrap()),
                2048
            );
            assert_ne!(&bpb[0x43..0x47], &0x12345678u32.to_le_bytes());

            let part = crate::copy::Partition::new(&mut f, base, p.part_sectors * 512);
            let fs = fatfs::FileSystem::new(part, fatfs::FsOptions::new()).unwrap();
            assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
            assert_eq!(fs.volume_label().trim_end(), "MY STICK 20");
            fs.root_dir().create_file("hello.txt").unwrap();
            drop(fs);
            let _ = std::fs::remove_file(&path);
        }
    }
}
