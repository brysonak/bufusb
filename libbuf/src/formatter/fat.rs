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

use anyhow::Result;
use fatfs::{FatType, FormatVolumeOptions};
use std::io::{Seek, SeekFrom, Write};

use super::Plan;
use crate::copy::{fat_label, random_guid, Partition, SectorCache};

pub(super) fn format(io: &mut SectorCache, p: &Plan) -> Result<()> {
    let mut part = Partition::new(io, p.part_start * p.sector, p.part_sectors * p.sector);
    let id = random_guid();
    let mut opts = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .bytes_per_sector(p.sector as u16)
        .bytes_per_cluster(p.cluster as u32)
        .volume_id(u32::from_le_bytes([id[0], id[1], id[2], id[3]]));
    if let Some(l) = &p.label {
        opts = opts.volume_label(fat_label(l));
    }
    fatfs::format_volume(&mut part, opts)
        .map_err(|e| anyhow::anyhow!("FAT32 format failed: {}", e))?;

    let hidden = (p.part_start as u32).to_le_bytes();
    for sec in [0, 6] {
        part.seek(SeekFrom::Start(sec * p.sector + 0x1C))?;
        part.write_all(&hidden)?;
    }
    Ok(())
}
