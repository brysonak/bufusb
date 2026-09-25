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


use anyhow::{bail, Result};
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dd,
    Copy,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Dd => "dd",
            Mode::Copy => "copy",
        })
    }
}

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dd" => Ok(Mode::Dd),
            "copy" => Ok(Mode::Copy),
            other => bail!("unknown mode '{}', expected 'dd' or 'copy'", other),
        }
    }
}

// What a source image is capable of
#[derive(Debug, Clone, Copy)]
pub struct ImageCaps {
    // 0x55AA boot marker at LBA0 means a raw write is bootable as authored
    pub dd_bootable: bool,
    // ISO9660 "CD001" magic at sector 16 means the tree is file-extractable
    pub is_iso: bool,
    // UDF NSR descriptor, some windows ISOs are UDF-only with no ISO9660 tree, OS mount handles it fine
    pub is_udf: bool,
}

impl ImageCaps {
    pub fn sniff(path: &Path) -> Result<Self> {
        let mut f = File::open(path)?;

        // MBR/VBR boot signature 
        let mut boot = [0u8; 2];
        let dd_bootable = f.seek(SeekFrom::Start(510)).is_ok()
            && f.read_exact(&mut boot).is_ok()
            && boot == [0x55, 0xAA];

        // https://wiki.osdev.org/ISO_9660#The_Primary_Volume_Descriptor
        let mut magic = [0u8; 5];
        let is_iso = f.seek(SeekFrom::Start(0x8001)).is_ok()
            && f.read_exact(&mut magic).is_ok()
            && &magic == b"CD001";

        // UDF descriptors are 2048 bytes starting at sector 16, id at byte 1, NSR02/NSR03 is UDF, other legal ids keep scanning, anything else stops
        let mut is_udf = false;
        for sect in 16u64..48 {
            let mut id = [0u8; 6];
            if f.seek(SeekFrom::Start(sect * 2048)).is_err() || f.read_exact(&mut id).is_err() {
                break;
            }
            match &id[1..6] {
                b"NSR02" | b"NSR03" => {
                    is_udf = true;
                    break;
                }
                b"BEA01" | b"TEA01" | b"BOOT2" | b"CD001" | b"CDW02" => continue,
                _ => break,
            }
        }

        Ok(Self { dd_bootable, is_iso, is_udf })
    }

    // copy mode needs a filesystem the OS can mount and we can read out of, ISO9660 or UDF
    pub fn copy_capable(&self) -> bool {
        self.is_iso || self.is_udf
    }
}

// Pick a mode when the user did not force one. Prefer copy whenever the image
// has an extractable filesystem, also for hybrid images. dd is the fallback for raw disk images with no extractable tree
pub fn auto(caps: ImageCaps) -> Result<Mode> {
    if caps.copy_capable() {
        return Ok(Mode::Copy);
    }
    if caps.dd_bootable {
        return Ok(Mode::Dd);
    }
    bail!(
        "Fatal: Source is neither an extractable ISO9660/UDF image nor a bootable disk \
         image (no 0x55AA marker); cannot auto-pick a mode. Use --mode to force one."
    )
}

// True if `chosen` was forced but the image only supports the other mode
// hybrid images will never trigger this
pub fn mode_risky(chosen: Mode, caps: ImageCaps) -> bool {
    let only_dd = caps.dd_bootable && !caps.copy_capable();
    let only_copy = caps.copy_capable() && !caps.dd_bootable;
    (only_dd && chosen != Mode::Dd) || (only_copy && chosen != Mode::Copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(dd: bool, iso: bool, udf: bool) -> ImageCaps {
        ImageCaps { dd_bootable: dd, is_iso: iso, is_udf: udf }
    }

    #[test]
    fn auto_prefers_copy_when_extractable() {
        assert_eq!(auto(caps(true, true, false)).unwrap(), Mode::Copy); // hybrid
        assert_eq!(auto(caps(false, true, false)).unwrap(), Mode::Copy);
        assert_eq!(auto(caps(false, false, true)).unwrap(), Mode::Copy); // UDF-only
    }

    #[test]
    fn auto_dd_for_raw_images() {
        assert_eq!(auto(caps(true, false, false)).unwrap(), Mode::Dd);
    }

    #[test]
    fn auto_errors_when_neither() {
        assert!(auto(caps(false, false, false)).is_err());
    }

    #[test]
    fn risky_only_when_single_capability_mismatched() {
        assert!(mode_risky(Mode::Copy, caps(true, false, false)));
        assert!(mode_risky(Mode::Dd, caps(false, true, false)));
        assert!(!mode_risky(Mode::Dd, caps(true, true, false))); // hybrid, never risky
        assert!(!mode_risky(Mode::Copy, caps(true, true, false)));
    }

    #[test]
    fn mode_parsing_roundtrip() {
        assert_eq!("DD".parse::<Mode>().unwrap(), Mode::Dd);
        assert_eq!(" copy ".parse::<Mode>().unwrap(), Mode::Copy);
        assert!("burn".parse::<Mode>().is_err());
        assert_eq!(Mode::Dd.to_string(), "dd");
        assert_eq!(Mode::Copy.to_string(), "copy");
    }
}
