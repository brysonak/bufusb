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


use anyhow::{Context, Result};
use chrono::Local;
use fern::Dispatch;
use log::LevelFilter;
use std::path::PathBuf;

fn home_dir() -> Option<PathBuf> {
    // When running under sudo, SUDO_USER holds the original username.
    // HOME at this point is root, so we need to get the real user's home
    // from passwd instead
    #[cfg(unix)]
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Some(home) = passwd_home(&sudo_user) {
            return Some(home);
        }
    }

    if let Ok(h) = std::env::var("HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        return Some(PathBuf::from(h));
    }
    None
}

#[cfg(unix)]
fn passwd_home(username: &str) -> Option<PathBuf> {
    use std::fs;
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut fields = line.splitn(7, ':');
        let name = fields.next()?;
        if name != username {
            continue;
        }
        let home = fields.nth(4)?;
        return Some(PathBuf::from(home));
    }
    None
}

pub fn log_path() -> Option<PathBuf> {
    let now = Local::now();
    // Colons are illegal in Windows filenames
    #[cfg(windows)]
    let filename = now.format("bufusb-%m-%d-%y-%H-%M-%S.log").to_string();
    #[cfg(not(windows))]
    let filename = now.format("bufusb-%m-%d-%y-%H:%M:%S.log").to_string();
    home_dir().map(|d| d.join(filename))
}

pub fn init(enabled: bool, verbose: bool, custom_path: Option<PathBuf>) -> Result<Option<PathBuf>> {
    let level = if verbose { LevelFilter::Debug } else { LevelFilter::Info };

    let formatter =
        |out: fern::FormatCallback, message: &std::fmt::Arguments, record: &log::Record| {
            out.finish(format_args!(
                "[{timestamp}] [{level:<5}] [{target}] {message}",
                timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                level = record.level(),
                target = record.target(),
                message = message,
            ))
        };

    if !enabled {
        Dispatch::new()
            .format(formatter)
            .level(LevelFilter::Warn)
            .chain(std::io::stderr())
            .apply()
            .context("Failed to initialise stderr logger")?;
        return Ok(None);
    }

    // Use the caller-supplied path if given, otherwise derive one from the
    // current timestamp in the user's home directory
    let path = match custom_path {
        Some(p) => p,
        None => match log_path() {
            Some(p) => p,
            None => {
                // HOME/USERPROFILE unset; can happen when running as SYSTEM after UAC elevation
                eprintln!("Warning: could not determine home directory, logging to stderr only");
                Dispatch::new()
                    .format(formatter)
                    .level(level)
                    .chain(std::io::stderr())
                    .apply()
                    .context("Failed to initialise stderr logger")?;
                return Ok(None);
            }
        },
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Could not create log directory: {}", parent.display()))?;
    }

    let log_file = fern::log_file(&path)
        .with_context(|| format!("Could not open log file: {}", path.display()))?;

    Dispatch::new()
        .format(formatter)
        .chain(Dispatch::new().level(level).chain(log_file))
        .chain(Dispatch::new().level(LevelFilter::Warn).chain(std::io::stderr()))
        .apply()
        .context("Failed to initialise logger")?;

    log::info!("bufusb logging started, file: {}", path.display());
    log::info!("Log level: {}", level);

    Ok(Some(path))
}
