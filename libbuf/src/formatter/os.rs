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
use std::process::Command;

use super::{Fs, Plan};
use crate::logger::{reason, run as run_logged};

#[cfg(unix)]
fn tool(fs: Fs) -> Option<(&'static str, &'static str)> {
    match fs {
        Fs::Ntfs => Some(("mkfs.ntfs", "ntfs-3g")),
        Fs::Ext4 => Some(("mkfs.ext4", "e2fsprogs")),
        Fs::Udf if cfg!(target_os = "macos") => Some(("newfs_udf", "macOS")),
        Fs::Udf => Some(("mkudffs", "udftools")),
        _ => None,
    }
}

pub(super) fn check_tool(fs: Fs) -> Result<()> {
    #[cfg(unix)]
    if let Some((bin, pkg)) = tool(fs) {
        if !crate::privilege::can_exec(bin) {
            bail!(
                "Fatal: {} needs {}, which wasn't found on PATH (it ships with {})",
                fs,
                bin,
                pkg
            );
        }
    }
    #[cfg(not(unix))]
    let _ = fs;
    Ok(())
}

fn run_tool(cmd: &mut Command, what: &str) -> Result<()> {
    crate::say!("  Formatting with {}...", what);
    let out = run_logged(cmd).with_context(|| format!("failed to run {}", what))?;
    if !out.status.success() {
        bail!("{} exited with {}: {}", what, out.status, reason(&out));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn format(p: &Plan, reread: bool) -> Result<()> {
    use crate::copy::ntfs::{partition_node, wait_for_node};

    if !reread {
        bail!(
            "Fatal: the kernel did not re-read {}'s new partition table, refusing to format a \
             stale partition. Close anything using the drive and retry",
            p.target
        );
    }
    let node = partition_node(std::path::Path::new(&p.target), 1);
    wait_for_node(&node)?;

    let (bin, _) = tool(p.fs).expect("available_here() only lets tool backed filesystems through");
    let mut cmd = Command::new(bin);
    let label = p.label.as_deref();
    match p.fs {
        Fs::Ntfs => cmd
            .args(["-f", "-F"])
            .args(label.map(|l| ["-L", l]).iter().flatten()),
        Fs::Ext4 => cmd
            .args(["-F", "-q"])
            .args(label.map(|l| ["-L", l]).iter().flatten()),
        Fs::Udf => cmd
            .args(["--media-type=hd", "--bootarea=erase"])
            .args(label.map(|l| format!("--label={}", l))),
        _ => unreachable!(),
    };
    run_tool(cmd.arg(&node), bin)
}

#[cfg(target_os = "macos")]
pub(super) fn format(p: &Plan, _reread: bool) -> Result<()> {
    let node = std::path::PathBuf::from(format!("{}s1", p.target));
    for _ in 0..40 {
        if node.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(125));
    }
    if !node.exists() {
        bail!("Partition node {} did not appear", node.display());
    }
    let mut cmd = Command::new("newfs_udf");
    if let Some(l) = &p.label {
        cmd.args(["-v", l]);
    }
    run_tool(cmd.arg(&node), "newfs_udf")
}

#[cfg(windows)]
pub(super) fn format(p: &Plan, _reread: bool) -> Result<()> {
    let n = crate::copy::physical_drive_number(std::path::Path::new(&p.target))
        .with_context(|| format!("Could not parse PhysicalDrive number from {}", p.target))?;
    let label = p.label.as_deref().unwrap_or("");
    let get = format!(
        "$ErrorActionPreference='Stop'; Update-HostStorageCache; \
         $p = Get-Partition -DiskNumber {n} -PartitionNumber 1; "
    );
    let letter =
        "if (-not $p.DriveLetter) { $p | Add-PartitionAccessPath -AssignDriveLetter | Out-Null; \
                  $p = Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber 1 }; ";
    let ps = match p.fs {
        Fs::Ntfs | Fs::Refs => format!(
            "{get}$p | Format-Volume -FileSystem {} -NewFileSystemLabel '{label}' -Confirm:$false | Out-Null; {letter}",
            p.fs
        ),
        Fs::Udf => format!(
            "{get}{letter}'Y' | format.com \"$($p.DriveLetter):\" /FS:UDF /Q /X /Y '/V:{label}'; \
             if ($LASTEXITCODE -ne 0) {{ throw \"format.com exited with $LASTEXITCODE\" }}"
        ),
        _ => unreachable!(),
    };
    run_tool(
        Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &ps]),
        "the Windows formatter",
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) fn format(p: &Plan, _reread: bool) -> Result<()> {
    bail!("Fatal: {} can't be created on this platform", p.fs)
}
