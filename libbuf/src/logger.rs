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

use chrono::Local;
use fern::Dispatch;
use indicatif::ProgressBar;
use log::{info, LevelFilter};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::Instant;

static BAR: Mutex<Option<ProgressBar>> = Mutex::new(None);

pub fn set_bar(pb: &ProgressBar) {
    *BAR.lock().unwrap_or_else(|e| e.into_inner()) = Some(pb.clone());
}

// On failure, freeze an unfinished bar where it stopped
pub fn abandon_bar() {
    if let Some(pb) = BAR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if !pb.is_finished() {
            pb.abandon();
            if !pb.is_hidden() {
                eprintln!();
            }
        }
    }
}

pub fn term(line: &str, stderr: bool) {
    let bar = BAR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let print = || {
        if stderr {
            eprintln!("{}", line)
        } else {
            println!("{}", line)
        }
    };
    match bar {
        Some(pb) if !pb.is_finished() => pb.suspend(print),
        _ => print(),
    }
}

#[macro_export]
macro_rules! say {
    () => { $crate::logger::term("", false) };
    ($($t:tt)*) => {{
        let m = format!($($t)*);
        if !m.trim().is_empty() {
            ::log::info!("{}", m.trim());
        }
        $crate::logger::term(&m, false);
    }};
}

#[cfg(unix)]
pub(crate) fn invoking_user() -> Option<nix::unistd::User> {
    use nix::unistd::{geteuid, Uid, User};
    if !geteuid().is_root() {
        return None;
    }
    ["SUDO_USER", "DOAS_USER"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find_map(|n| User::from_name(&n).ok().flatten())
        .or_else(|| {
            let uid = std::env::var("PKEXEC_UID").ok()?.parse().ok()?;
            User::from_uid(Uid::from_raw(uid)).ok().flatten()
        })
        .filter(|u| !u.uid.is_root())
}

fn log_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })
        .map(|d| d.join("bufusb").join("logs"));

    #[cfg(unix)]
    {
        let user = invoking_user();
        let home = user
            .as_ref()
            .map(|u| u.dir.clone())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;

        #[cfg(target_os = "macos")]
        return Some(home.join("Library").join("Logs").join("bufusb"));

        #[cfg(not(target_os = "macos"))]
        return Some(
            std::env::var_os("XDG_STATE_HOME")
                .filter(|s| user.is_none() && !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local").join("state"))
                .join("bufusb"),
        );
    }

    #[cfg(not(any(unix, windows)))]
    None
}

pub fn log_path() -> Option<PathBuf> {
    Some(
        log_dir()?.join(
            Local::now()
                .format("bufusb-%Y-%m-%dT%H-%M-%S.log")
                .to_string(),
        ),
    )
}

#[cfg(unix)]
fn give_back(path: &Path, first_new_dir: Option<&Path>) {
    let Some(u) = invoking_user() else { return };
    if !path.starts_with(&u.dir) {
        return;
    }
    let dirs = path
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|d| first_new_dir.is_some_and(|f| d.starts_with(f)));
    for p in std::iter::once(path).chain(dirs) {
        if let Err(e) = nix::unistd::chown(p, Some(u.uid), Some(u.gid)) {
            log::debug!("chown {} to {} failed: {}", p.display(), u.name, e);
        }
    }
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let first_new = dir
        .and_then(|d| d.ancestors().take_while(|a| !a.exists()).last())
        .map(Path::to_path_buf);
    if let Some(d) = dir {
        std::fs::create_dir_all(d)?;
    }
    let file = fern::log_file(path)?;
    #[cfg(unix)]
    give_back(path, first_new.as_deref());
    #[cfg(not(unix))]
    let _ = first_new;
    Ok(file)
}

pub fn init(enabled: bool, verbose: bool, custom_path: Option<PathBuf>) -> Option<PathBuf> {
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let stderr = Dispatch::new()
        .level(LevelFilter::Warn)
        .filter(|m| m.target() != "panic")
        .format(|out, msg, rec| {
            let tag = if rec.level() == log::Level::Error {
                "error"
            } else {
                "warning"
            };
            out.finish(format_args!("{}: {}", tag, msg))
        })
        .chain(fern::Output::call(|rec| {
            term(&rec.args().to_string(), true)
        }));

    let (path, file) = match enabled.then(|| custom_path.or_else(log_path)).flatten() {
        Some(p) => {
            match open_log(&p) {
                Ok(f) => (Some(p), Some(f)),
                Err(e) => {
                    term(&format!("warning: could not open log file {} ({}), logging to the terminal only", p.display(), e), true);
                    (None, None)
                }
            }
        }
        None => {
            if enabled {
                term(
                    "warning: could not determine a log directory, logging to the terminal only",
                    true,
                );
            }
            (None, None)
        }
    };

    let mut root = Dispatch::new()
        .level_for("fatfs", LevelFilter::Info)
        .chain(stderr);
    if let Some(f) = file {
        root = root.chain(
            Dispatch::new()
                .level(level)
                .format(|out, msg, rec| {
                    out.finish(format_args!(
                        "[{}] [{:<5}] [{}] [{}] {}",
                        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                        rec.level(),
                        std::process::id(),
                        rec.target(),
                        msg,
                    ))
                })
                .chain(f),
        );
    }
    let _ = root.apply();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        log::error!(target: "panic", "{}", p);
        default_hook(p);
    }));

    if let Some(ref p) = path {
        info!("Log file: {} (level {})", p.display(), level);
    }
    path
}

pub fn log_context() {
    let cmd = std::env::args_os()
        .map(|a| {
            let s = a.to_string_lossy().into_owned();
            if s.is_empty() || s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
                format!("{:?}", s)
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    info!(
        "bufusb {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    info!("Command: {}", cmd);
    info!(
        "Working directory: {}",
        std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|e| e.to_string())
    );
    info!("Privileged: {}", crate::is_privileged());

    #[cfg(unix)]
    if let Some(u) = invoking_user() {
        info!("Invoked by user {} (uid {})", u.name, u.uid);
    }

    #[cfg(target_os = "linux")]
    {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let distro = std::fs::read_to_string("/etc/os-release")
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_default();
        info!("OS: {} (kernel {})", distro, kernel.trim());
    }
}

pub(crate) fn run(cmd: &mut Command) -> std::io::Result<Output> {
    info!("exec: {:?}", cmd);
    let start = Instant::now();
    let out = cmd.output();
    match &out {
        Ok(o) => {
            info!("exec: {} after {:.2?}", o.status, start.elapsed());
            for (name, bytes) in [("stdout", &o.stdout), ("stderr", &o.stderr)] {
                let s = String::from_utf8_lossy(bytes);
                if !s.trim().is_empty() {
                    info!("exec {}:\n{}", name, s.trim_end());
                }
            }
        }
        Err(e) => info!("exec: could not start: {}", e),
    }
    out
}

pub(crate) fn reason(o: &Output) -> String {
    let raw = if o.stderr.iter().all(u8::is_ascii_whitespace) {
        &o.stdout
    } else {
        &o.stderr
    };
    pick_reason(&String::from_utf8_lossy(raw))
}

fn pick_reason(s: &str) -> String {
    const HINTS: [&str; 9] = [
        "fail",
        "error",
        "denied",
        "cannot",
        "can't",
        "unable",
        "invalid",
        "not found",
        "no such",
    ];
    let lines: Vec<&str> = s
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with('+')
                && !l.starts_with("At line:")
                && !l.starts_with("dmesg(1)")
        })
        .collect();
    lines
        .iter()
        .rev()
        .find(|l| HINTS.iter().any(|h| l.to_ascii_lowercase().contains(h)))
        .or(lines.first())
        .map_or(String::new(), |l| l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandon_bar_finishes_and_forgets_the_bar() {
        let pb = ProgressBar::hidden();
        set_bar(&pb);
        abandon_bar();
        assert!(pb.is_finished());
        assert!(BAR.lock().unwrap().is_none());
    }

    #[test]
    fn log_name_sorts_and_has_no_colons() {
        let p = log_path().expect("HOME or LOCALAPPDATA is set in tests");
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("bufusb-20") && name.ends_with(".log") && !name.contains(':'),
            "{}",
            name
        );
        assert!(p.parent().unwrap().ends_with("bufusb") || p.parent().unwrap().ends_with("logs"));
    }

    #[test]
    #[cfg(unix)]
    fn run_captures_output_and_reason_falls_back() {
        let o =
            run(Command::new("sh").args(["-c", "echo out; echo first >&2; echo last >&2; exit 3"]))
                .unwrap();
        assert_eq!(o.status.code(), Some(3));
        assert_eq!(reason(&o), "first");
        let o = run(Command::new("sh").args(["-c", "echo only-stdout"])).unwrap();
        assert_eq!(reason(&o), "only-stdout");
    }

    #[test]
    fn reason_skips_boilerplate() {
        let ntfs3g = "ntfs_mst_post_read_fixup_warn: magic: 0x00000000  size: 1024   usa_ofs: 0  usa_count: 0: Invalid argument
Record 0 has no FILE magic (0x0)
Failed to mount '/dev/sda1': Input/output error
NTFS is either inconsistent, or there is a hardware fault, or it's a
SoftRAID/FakeRAID hardware. In the first case run chkdsk /f on Windows
/dev/mapper/nvidia_eahaabcc1). Please see the 'dmraid' documentation
for more details.";
        assert_eq!(
            pick_reason(ntfs3g),
            "Failed to mount '/dev/sda1': Input/output error"
        );

        let ntfs3 = "mount: /tmp/x: wrong fs type, bad option, bad superblock on /dev/sda1, missing codepage or helper program, or other error.
       dmesg(1) may have more information after failed mount system call.";
        assert!(pick_reason(ntfs3).starts_with("mount: /tmp/x: wrong fs type"));

        let ps = "Format-Volume : Access to a CIM resource was not available to the client.
At line:1 char:72
+ ... | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'X' -Confirm:$false
+       ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
    + CategoryInfo          : PermissionDenied: (MSFT_Partition:ROOT/Microsoft/Windows/Storage/MSFT_Partition) [Format-Volume], CimException
    + FullyQualifiedErrorId : MI RESULT 2,Format-Volume";
        assert_eq!(
            pick_reason(ps),
            "Format-Volume : Access to a CIM resource was not available to the client."
        );

        assert_eq!(pick_reason(""), "");
    }

    #[test]
    #[cfg(unix)]
    fn open_log_creates_dirs_and_appends() {
        let base = std::env::temp_dir().join(format!("buf-log-test-{}", std::process::id()));
        let p = base.join("a/b/run.log");
        use std::io::Write;
        open_log(&p).unwrap().write_all(b"one\n").unwrap();
        open_log(&p).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\n");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
