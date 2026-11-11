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

// https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification

use anyhow::Result;
use log::info;
use std::io::{Seek, SeekFrom, Write};

use super::Plan;
use crate::copy::{random_guid, Partition, SectorCache};

const UPCASE: &[u8] = include_bytes!("../../assets/exfat-upcase.bin");

pub(super) fn cluster_size(part_bytes: u64) -> u64 {
    match part_bytes {
        b if b < 256 << 20 => 4096,
        b if b <= 32 << 30 => 32 * 1024,
        _ => 128 * 1024,
    }
}

#[derive(Debug)]
struct Layout {
    fat_off: u64,
    fat_len: u64,
    heap_off: u64,
    clusters: u64,
    bitmap_len: u64,
    bitmap_at: u64,
    upcase_at: u64,
    root_at: u64,
    used: u64,
}

fn layout(p: &Plan) -> Layout {
    let spc = p.cluster / p.sector;
    let align = (1024 * 1024).max(p.cluster) / p.sector;
    let fat_off = align;
    let fat_len = ((p.part_sectors / spc + 2) * 4).div_ceil(p.sector);
    let heap_off = (fat_off + fat_len).next_multiple_of(align);
    let clusters = (p.part_sectors - heap_off) / spc;
    let bitmap_len = clusters.div_ceil(8);
    let bitmap_cl = bitmap_len.div_ceil(p.cluster);
    let upcase_cl = (UPCASE.len() as u64).div_ceil(p.cluster);
    Layout {
        fat_off,
        fat_len,
        heap_off,
        clusters,
        bitmap_len,
        bitmap_at: 2,
        upcase_at: 2 + bitmap_cl,
        root_at: 2 + bitmap_cl + upcase_cl,
        used: bitmap_cl + upcase_cl + 1,
    }
}

pub(super) fn format(io: &mut SectorCache, p: &Plan) -> Result<()> {
    let l = layout(p);
    info!("exFAT layout: {:?}", l);
    let s = p.sector;
    let mut part = Partition::new(io, p.part_start * s, p.part_sectors * s);

    let zero = vec![0u8; 1 << 20];
    let mut left = (l.heap_off + l.used * p.cluster / s) * s;
    part.seek(SeekFrom::Start(0))?;
    while left > 0 {
        let n = left.min(zero.len() as u64) as usize;
        part.write_all(&zero[..n])?;
        left -= n as u64;
    }

    let g = random_guid();
    let boot = boot_region(p, &l, u32::from_le_bytes([g[0], g[1], g[2], g[3]]));
    part.seek(SeekFrom::Start(0))?;
    part.write_all(&boot)?;
    part.write_all(&boot)?;

    let mut fat: Vec<u8> = [0xFFFF_FFF8u32, 0xFFFF_FFFF]
        .iter()
        .flat_map(|e| e.to_le_bytes())
        .collect();
    for (start, end) in [
        (l.bitmap_at, l.upcase_at),
        (l.upcase_at, l.root_at),
        (l.root_at, l.root_at + 1),
    ] {
        for c in start..end {
            let next = if c + 1 == end {
                0xFFFF_FFFF
            } else {
                c as u32 + 1
            };
            fat.extend(next.to_le_bytes());
        }
    }
    part.seek(SeekFrom::Start(l.fat_off * s))?;
    part.write_all(&fat)?;

    let cluster_at = |c: u64| SeekFrom::Start(l.heap_off * s + (c - 2) * p.cluster);

    let mut bitmap = vec![0u8; l.bitmap_len as usize];
    for i in 0..l.used as usize {
        bitmap[i / 8] |= 1 << (i % 8);
    }
    part.seek(cluster_at(l.bitmap_at))?;
    part.write_all(&bitmap)?;

    part.seek(cluster_at(l.upcase_at))?;
    part.write_all(UPCASE)?;

    let mut root = Vec::with_capacity(96);
    if let Some(label) = &p.label {
        let mut e = [0u8; 32];
        e[0] = 0x83;
        for (i, u) in label.encode_utf16().enumerate() {
            e[1] += 1;
            e[2 + i * 2..4 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        root.extend(e);
    }
    let mut e = [0u8; 32];
    e[0] = 0x81;
    e[20..24].copy_from_slice(&(l.bitmap_at as u32).to_le_bytes());
    e[24..32].copy_from_slice(&l.bitmap_len.to_le_bytes());
    root.extend(e);
    let mut e = [0u8; 32];
    e[0] = 0x82;
    e[4..8].copy_from_slice(&checksum(UPCASE, &[]).to_le_bytes());
    e[20..24].copy_from_slice(&(l.upcase_at as u32).to_le_bytes());
    e[24..32].copy_from_slice(&(UPCASE.len() as u64).to_le_bytes());
    root.extend(e);
    part.seek(cluster_at(l.root_at))?;
    part.write_all(&root)?;
    Ok(())
}

fn boot_region(p: &Plan, l: &Layout, serial: u32) -> Vec<u8> {
    let s = p.sector as usize;
    let mut r = vec![0u8; 12 * s];
    let b = &mut r[..512];
    b[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(b"EXFAT   ");
    b[64..72].copy_from_slice(&p.part_start.to_le_bytes());
    b[72..80].copy_from_slice(&p.part_sectors.to_le_bytes());
    for (off, v) in [
        (80, l.fat_off),
        (84, l.fat_len),
        (88, l.heap_off),
        (92, l.clusters),
        (96, l.root_at),
        (100, serial as u64),
    ] {
        b[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
    }
    b[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    b[108] = p.sector.trailing_zeros() as u8;
    b[109] = (p.cluster / p.sector).trailing_zeros() as u8;
    b[110] = 1; // one FAT
    b[111] = 0x80; // drive select
    b[510] = 0x55;
    b[511] = 0xAA;
    for i in 1..=8 {
        r[(i + 1) * s - 4..(i + 1) * s].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    }
    let sum = checksum(&r[..11 * s], &[106, 107, 112]);
    for c in r[11 * s..].chunks_mut(4) {
        c.copy_from_slice(&sum.to_le_bytes());
    }
    r
}

fn checksum(data: &[u8], skip: &[usize]) -> u32 {
    data.iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .fold(0u32, |c, (_, &b)| c.rotate_right(1).wrapping_add(b as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formatter::{plan, write, Fs, Table};
    use std::fs::File;
    use std::io::Read;

    #[test]
    fn upcase_table_is_the_spec_one() {
        assert_eq!(UPCASE.len(), 5836);
        assert_eq!(checksum(UPCASE, &[]), 0xE619_D30D);
    }

    #[test]
    fn formats_a_volume_that_reads_back() {
        let path = std::env::temp_dir().join(format!("buf-exfat-test-{}.img", std::process::id()));
        File::create(&path).unwrap().set_len(300 << 20).unwrap();
        let target = path.to_string_lossy().into_owned();
        let p = plan(&target, Fs::Exfat, Some(Table::Mbr), Some("Bryson USB!")).unwrap();
        write(&p, false).unwrap();

        let mut f = File::open(&path).unwrap();
        let mut img = Vec::new();
        f.read_to_end(&mut img).unwrap();
        let _ = std::fs::remove_file(&path);
        let vol = &img[(p.part_start * 512) as usize..];
        let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());

        assert_eq!(img[450], 0x07, "MBR type");
        assert_eq!(&vol[3..11], b"EXFAT   ");
        assert_eq!(
            vol[..12 * 512],
            vol[12 * 512..24 * 512],
            "backup boot region"
        );
        assert_eq!(
            u32_at(vol, 11 * 512),
            checksum(&vol[..11 * 512], &[106, 107, 112])
        );

        let (fat_off, heap, root) = (u32_at(vol, 80), u32_at(vol, 88), u32_at(vol, 96));
        let cs = 512usize << vol[109];
        assert_eq!(cs as u64, p.cluster);
        let fat = |c: u32| u32_at(vol, fat_off as usize * 512 + c as usize * 4);
        let clus = |c: u32| &vol[heap as usize * 512 + (c as usize - 2) * cs..][..cs];
        assert_eq!(fat(root), 0xFFFF_FFFF);

        let dir = clus(root);
        assert_eq!(dir[0], 0x83);
        assert_eq!(dir[1], 11);
        assert_eq!(&dir[2..6], &[b'B', 0, b'r', 0]);
        assert_eq!(dir[32], 0x81);
        let bitmap = clus(u32_at(dir, 52));
        assert_eq!(bitmap[0], 0b111, "bitmap, up-case and root marked used");
        assert_eq!(dir[64], 0x82);
        let up = u32_at(dir, 84);
        assert_eq!(&clus(up)[..UPCASE.len()], UPCASE);
        assert_eq!(u32_at(dir, 68), 0xE619_D30D);
    }
}
