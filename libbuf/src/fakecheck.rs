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

// Counterfeit drive check
// Note: Drives with big internal write caches might not work. Use f3probe for those

use anyhow::{bail, Context, Result};
use log::{debug, info};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

use crate::list::human_bytes;

const MARKERS: u64 = 32;
const MAGIC: &[u8; 8] = b"BUFUSBFK";

#[derive(Debug, PartialEq)]
struct Bad {
    last_good: Option<u64>,
    first_bad: u64,
}

pub fn check(target: &Path, start: u64, end: u64) -> Result<()> {
    if !is_device(target) {
        info!(
            "Fake-drive check skipped, {} is not a device",
            target.display()
        );
        return Ok(());
    }
    let bs = crate::writer::get_sector_size(target).max(4096);
    if end.saturating_sub(start) < 2 * bs as u64 {
        return Ok(());
    }
    crate::say!("  Checking that the drive really stores its full size...");
    let t = Instant::now();

    let file = crate::validate::open_target_file(target).with_context(|| {
        format!(
            "Could not open {} for the fake-drive check",
            target.display()
        )
    })?;
    let mut dev = Synced(file);
    let mut wbuf = crate::writer::AlignedBuffer::new(bs, bs);
    let mut rbuf = crate::writer::AlignedBuffer::new(bs, bs);
    let res = probe(
        &mut dev,
        start,
        end,
        nonce(),
        wbuf.as_mut_slice_n(bs),
        rbuf.as_mut_slice_n(bs),
    )?;
    info!("Fake-drive check took {:.2?}", t.elapsed());

    let Some(bad) = res else {
        info!(
            "Fake-drive check passed for {} .. {}",
            human_bytes(start),
            human_bytes(end)
        );
        return Ok(());
    };
    let claim = human_bytes(crate::device_size(target).unwrap_or(end));
    match bad.last_good {
        Some(good) => bail!(
            "Fatal: {} looks counterfeit. It claims {} but only stores somewhere between {} and {} \
             of it. Use a different drive, or pass --no-fake-check to flash anyway",
            target.display(),
            claim,
            human_bytes(good),
            human_bytes(bad.first_bad),
        ),
        None => bail!(
            "Fatal: {} does not keep the data written to it (test data at {} read back wrong). \
             The drive is counterfeit or failing. Use a different drive, or pass --no-fake-check \
             to flash anyway",
            target.display(),
            human_bytes(bad.first_bad),
        ),
    }
}

fn probe<D: Read + Write + Seek>(
    dev: &mut D,
    start: u64,
    end: u64,
    nonce: u64,
    wbuf: &mut [u8],
    rbuf: &mut [u8],
) -> Result<Option<Bad>> {
    let bs = wbuf.len() as u64;
    let pos = positions(start, end, bs);

    let mut wrote = vec![false; pos.len()];
    for (i, &p) in pos.iter().enumerate() {
        fill(wbuf, nonce, p);
        let w = dev
            .seek(SeekFrom::Start(p))
            .and_then(|_| dev.write_all(wbuf));
        if let Err(e) = &w {
            debug!("fake-drive check: write at {} failed: {}", p, e);
        }
        wrote[i] = w.is_ok();
    }
    if let Err(e) = dev.flush() {
        debug!("fake-drive check: sync failed: {}", e);
    }

    let mut ok = vec![false; pos.len()];
    for (i, &p) in pos.iter().enumerate().rev() {
        fill(wbuf, nonce, p);
        let read = dev
            .seek(SeekFrom::Start(p))
            .and_then(|_| dev.read_exact(rbuf));
        ok[i] = wrote[i] && read.is_ok() && rbuf == &*wbuf;
        debug!(
            "fake-drive check: marker at {} {}",
            p,
            if ok[i] { "ok" } else { "BAD" }
        );
    }

    Ok(ok.iter().position(|o| !o).map(|i| Bad {
        last_good: i.checked_sub(1).map(|g| pos[g]),
        first_bad: pos[i],
    }))
}

fn positions(start: u64, end: u64, bs: u64) -> Vec<u64> {
    let first = start.div_ceil(bs) * bs;
    let last = (end / bs).saturating_sub(1) * bs;
    let mut v: Vec<u64> = (0..=MARKERS)
        .map(|k| (first + (last - first) * k / MARKERS) / bs * bs)
        .collect();
    v.dedup();
    v
}

fn fill(buf: &mut [u8], nonce: u64, pos: u64) {
    let mut x = nonce ^ pos.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    buf[..8].copy_from_slice(MAGIC);
    buf[8..16].copy_from_slice(&nonce.to_le_bytes());
    buf[16..24].copy_from_slice(&pos.to_le_bytes());
}

fn nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

fn is_device(target: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(target)
            .map(|m| m.file_type().is_block_device() || m.file_type().is_char_device())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        target.to_string_lossy().starts_with(r"\\.\")
    }
}

struct Synced(std::fs::File);

impl Read for Synced {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for Synced {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.sync_data()
    }
}
impl Seek for Synced {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    struct Fake {
        mem: Vec<u8>,
        claimed: u64,
        wrap: bool,
        pos: u64,
    }

    impl Fake {
        fn new(real: usize, claimed: u64, wrap: bool) -> Self {
            Fake {
                mem: vec![0; real],
                claimed,
                wrap,
                pos: 0,
            }
        }
        fn at(&self, p: u64) -> Option<usize> {
            let real = self.mem.len() as u64;
            match (p < real, self.wrap) {
                (true, _) => Some(p as usize),
                (false, true) => Some((p % real) as usize),
                (false, false) => None,
            }
        }
    }

    impl Read for Fake {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            for (i, out) in b.iter_mut().enumerate() {
                *out = self.at(self.pos + i as u64).map_or(0, |a| self.mem[a]);
            }
            self.pos += b.len() as u64;
            Ok(b.len())
        }
    }
    impl Write for Fake {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            for (i, &v) in b.iter().enumerate() {
                if let Some(a) = self.at(self.pos + i as u64) {
                    self.mem[a] = v;
                }
            }
            self.pos += b.len() as u64;
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Fake {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            let SeekFrom::Start(p) = p else {
                unreachable!()
            };
            assert!(p < self.claimed);
            self.pos = p;
            Ok(p)
        }
    }

    const BS: usize = 4096;
    const MIB: u64 = 1 << 20;

    fn run<D: Read + Write + Seek>(d: &mut D, end: u64) -> Option<Bad> {
        let (mut w, mut r) = (vec![0u8; BS], vec![0u8; BS]);
        probe(d, 0, end, 0xC0FFEE, &mut w, &mut r).unwrap()
    }

    #[test]
    fn genuine_drive_passes() {
        let mut d = Cursor::new(vec![0u8; 64 * MIB as usize]);
        assert_eq!(run(&mut d, 64 * MIB), None);
    }

    #[test]
    fn limbo_fake_is_caught_with_its_rough_real_size() {
        let mut d = Fake::new(5 * MIB as usize, 32 * MIB, false);
        let bad = run(&mut d, 32 * MIB).expect("limbo fake must fail");
        let good = bad.last_good.unwrap();
        assert!(good < 5 * MIB && bad.first_bad >= 5 * MIB, "{:?}", bad);
        assert!(
            5 * MIB - good <= 32 * MIB / MARKERS,
            "estimate within one marker gap"
        );
    }

    #[test]
    fn wraparound_fake_is_caught() {
        let mut d = Fake::new(5 * MIB as usize, 32 * MIB, true);
        assert!(run(&mut d, 32 * MIB).is_some());
    }

    #[test]
    fn range_inside_the_real_part_passes_on_a_fake() {
        let mut d = Fake::new(5 * MIB as usize, 32 * MIB, false);
        assert_eq!(run(&mut d, 4 * MIB), None);
    }

    #[test]
    fn positions_are_aligned_in_range_and_unique() {
        let p = positions(1000, 10 * MIB + 123, BS as u64);
        assert_eq!(p[0], BS as u64);
        assert!(p
            .iter()
            .all(|x| x % BS as u64 == 0 && x + BS as u64 <= 10 * MIB + 123));
        assert!(p.windows(2).all(|w| w[0] < w[1]));
    }
}
