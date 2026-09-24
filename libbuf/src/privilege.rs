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


use anyhow::Result;
use log::{debug, info, warn};

pub fn is_privileged() -> bool {
    #[cfg(unix)]
    {
        use nix::unistd::geteuid;
        let euid = geteuid();
        debug!("EUID = {}", euid);
        return euid.is_root();
    }

    #[cfg(windows)]
    {
        return windows_is_elevated();
    }

    #[cfg(not(any(unix, windows)))]
    {
        warn!("Cannot determine privilege level on this platform, assuming OK");
        return true;
    }
}

pub fn elevate_or_warn(args: &[String]) -> Result<()> {
    warn!("bufusb must be run as root/Administrator to write to block devices");
    eprintln!("\n  bufusb requires elevated privileges to write to block devices.\n");

    #[cfg(unix)]
    return unix_elevate(args);

    #[cfg(windows)]
    return windows_elevate(args);

    #[cfg(not(any(unix, windows)))]
    {
        eprintln!("  Please re-run bufusb with administrator/root privileges.");
        std::process::exit(1);
    }
}

#[cfg(unix)]
fn unix_elevate(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("bufusb"));

    const CANDIDATES: [&str; 4] = ["doas", "sudo", "run0", "pkexec"];

    let escalator = match std::env::var("BUF_SUDO") {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim().to_string();
            if !can_exec(&v) {
                eprintln!("BUF_SUDO is set to '{}' but that is not an executable.", v);
                std::process::exit(1);
            }
            v
        }
        _ => match CANDIDATES.iter().find(|c| can_exec(c)) {
            Some(c) => c.to_string(),
            None => {
                eprintln!(
                    "No escalation tool found (tried {}). Re-run bufusb as root, or set \
                     BUF_SUDO to the one you use.",
                    CANDIDATES.join(", ")
                );
                std::process::exit(1);
            }
        },
    };

    eprintln!("  Attempting to re-launch via {}...\n", escalator);
    info!("Re-launching via {} {:?} {:?}", escalator, exe, args);

    let status = std::process::Command::new(&escalator)
        .arg(&exe)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to spawn {}: {}", escalator, e))?;

    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(unix)]
fn can_exec(cmd: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let executable = |p: &std::path::Path| {
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };

    // A name containing a separator is a path already, don't search PATH for it
    if cmd.contains('/') {
        return executable(std::path::Path::new(cmd));
    }

    // Check PATH entries directly rather than shelling out to `which`
    std::env::var("PATH")
        .map(|p| p.split(':').any(|d| executable(&std::path::Path::new(d).join(cmd))))
        .unwrap_or(false)
}

#[cfg(windows)]
fn windows_is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size,
            &mut size,
        );
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

#[cfg(windows)]
fn windows_elevate(args: &[String]) -> Result<()> {
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("bufusb.exe"));
    let cmd_params = format!("/k \"{}\" {}", exe.to_string_lossy(), args.join(" "));

    eprintln!("  Requesting UAC elevation...\n");
    info!("Requesting UAC elevation for {:?} with args {:?}", exe, args);

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(once(0)).collect()
    }

    let verb = to_wide("runas");
    let file = to_wide("cmd.exe");
    let param = to_wide(&cmd_params);

    unsafe {
        ShellExecuteW(
            None,
            windows::core::PCWSTR(verb.as_ptr()),
            windows::core::PCWSTR(file.as_ptr()),
            windows::core::PCWSTR(param.as_ptr()),
            None,
            SW_SHOW,
        );
    }

    std::process::exit(0);
}
