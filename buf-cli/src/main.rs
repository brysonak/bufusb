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
use clap::{ArgAction, Parser};
use libbuf::{say, Mode};
use log::{debug, error, info, warn};

#[derive(Parser, Debug)]
#[command(
    name       = "bufusb",
    version    = "0.2.6",
    long_version = "0.2.6\n Copyright (C) 2026  Bryson Kelly\n    This program comes with ABSOLUTELY NO WARRANTY; for details, visit: https://github.com/brysonak/bufusb/blob/main/LICENSE\n    This is free software, and you are welcome to redistribute it\n    under certain conditions.",
    author     = "Bryson Kelly",
    about      = "A fast, safe bootable USB image flasher",
    long_about = None,
    after_help = "For full documentation, please visit: https://github.com/brysonak/bufusb/blob/main/docs/docs.md",
    styles     = clap_styles(),
)]
struct Cli {
    #[arg(
        short = 's',
        long = "source",
        value_name = "FILE",
        help = "Source ISO/IMG file to flash"
    )]
    source: Option<String>,

    // I cannot stress enough, I *fucking* HATE the way windows does drive naming...
    // `\\.\PhysicalDriveN`.... What a stupid convention
    #[arg(
        short = 't',
        long = "target",
        value_name = "DEVICE",
        help = "Target block device (e.g. /dev/sdb, /dev/disk2, or \\\\.\\PhysicalDrive1)"
    )]
    target: Option<String>,

    #[arg(
        short = 'l',
        long = "list",
        action = ArgAction::SetTrue,
        help = "List storage devices and exit"
    )]
    list: bool,

    #[arg(
        short = 'm',
        long = "mode",
        value_name = "MODE",
        action = ArgAction::Append,
        value_delimiter = ',',
        help = "Write mode: 'dd' (raw image) or 'copy' (extract ISO files). Default: auto-detect"
    )]
    mode: Vec<String>,

    #[arg(
        long = "label",
        value_name = "NAME",
        help = "Volume label for the flashed drive, copy mode only (default: the ISO's own label)"
    )]
    label: Option<String>,

    #[arg(
        short = 'b',
        long = "block-size",
        value_name = "SIZE",
        default_value = "32MiB",
        help = "Write block size, dd mode only (default: 32MiB)"
    )]
    block_size: String,

    #[arg(
        long = "offset",
        value_name = "BYTES",
        default_value_t = 0,
        help = "Start writing at this byte offset into the target (dd mode only)"
    )]
    offset: u64,

    #[arg(
        short = 'f',
        long = "force",
        action = ArgAction::SetTrue,
        help = "Skip the confirmation prompt"
    )]
    force: bool,

    #[arg(
        long = "dry-run",
        action = ArgAction::SetTrue,
        help = "Validate everything without writing any data"
    )]
    dry_run: bool,

    #[arg(
        long = "no-logging",
        short_alias = 'n',
        action = ArgAction::SetTrue,
        help = "Disable log file creation"
    )]
    no_logging: bool,

    #[arg(
        long = "log-path",
        value_name = "PATH",
        help = "Write the log file to this path (default: a timestamped file in the per-user log directory, see docs)"
    )]
    log_path: Option<String>,

    #[arg(
        short = 'v',
        long = "verbose",
        action = ArgAction::SetTrue,
        help = "Enable verbose debug logging"
    )]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();

    if cli.no_logging && cli.log_path.is_some() {
        eprintln!("error: --log-path and --no-logging cannot be used together");
        std::process::exit(1);
    }

    let custom_log_path = cli.log_path.as_deref().map(|p| {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.into())
    });

    let file_logging = !cli.no_logging && (!cli.list || custom_log_path.is_some());
    let log_path = libbuf::init_logger(file_logging, cli.verbose, custom_log_path);
    if let Some(ref path) = log_path {
        println!("Logging to: {}", path.display());
    }
    libbuf::logger::log_context();
    debug!("Parsed CLI args: {:?}", cli);

    let start = std::time::Instant::now();
    let result = run(cli, log_path.as_deref());
    match result {
        Ok(()) => info!("Finished OK after {:.1?}", start.elapsed()),
        Err(e) => {
            error!("{:#}", e);
            info!("Failed after {:.1?}", start.elapsed());
            if let Some(p) = log_path {
                eprintln!("The full log is at {}", p.display());
            }
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli, log_path: Option<&std::path::Path>) -> Result<()> {
    if cli.list {
        let devices = libbuf::list_drives()?;
        libbuf::print_device_table(&devices);
        return Ok(());
    }

    let source = cli
        .source
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Fatal: --source/-s is required. Use --help for usage."))?;

    let target = cli
        .target
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Fatal: --target/-t is required. Use --help for usage."))?;

    // Parse --mode early. If the user asked for both dd and copy at once, stop before we prompt for elevation or touch the device
    let requested = parse_modes(&cli.mode)?;
    if requested.len() >= 2 {
        bail!("Fatal: Cannot use both dd and copy modes at the same time, stopping...");
    }
    let requested: Option<Mode> = requested.first().copied();

    if let Some(ref l) = cli.label {
        validate_label(l)?;
    }

    // Resolve to absolute before elevation. UAC relaunch via cmd.exe resets the
    // working directory to System32, so a relative path would not resolve correctly.
    let source = {
        let p = std::path::Path::new(&source);
        if p.is_absolute() {
            source
        } else {
            std::fs::canonicalize(p)
                .with_context(|| format!("Could not resolve source path: {}", source))?
                .to_string_lossy()
                .to_string()
        }
    };

    #[cfg(unix)]
    let target = std::fs::canonicalize(&target)
        .with_context(|| format!("Could not resolve target path: {}", target))?
        .to_string_lossy()
        .into_owned();
    info!("Source: {}", source);
    info!("Target: {}", target);

    if !libbuf::is_privileged() {
        info!("Not running as root/Administrator, relaunching elevated");
        let mut argv = vec![
            "--source".to_string(), source.clone(),
            "--target".to_string(), target.clone(),
        ];
        for m in &cli.mode {
            argv.extend(["--mode".to_string(), m.clone()]);
        }
        if cli.block_size != "32MiB" {
            argv.extend(["--block-size".to_string(), cli.block_size.clone()]);
        }
        if cli.offset != 0 {
            argv.extend(["--offset".to_string(), cli.offset.to_string()]);
        }
        if let Some(p) = log_path {
            argv.extend(["--log-path".to_string(), p.to_string_lossy().into_owned()]);
        }
        if let Some(ref l) = cli.label {
            argv.extend(["--label".to_string(), l.clone()]);
        }
        if cli.force      { argv.push("--force".to_string()); }
        if cli.dry_run    { argv.push("--dry-run".to_string()); }
        if cli.no_logging { argv.push("--no-logging".to_string()); }
        if cli.verbose    { argv.push("--verbose".to_string()); }
        libbuf::elevate_or_warn(&argv)?;
    }

    log_target_device(&target);

    // Sniff the image and settle on a single mode
    let caps = libbuf::ImageCaps::sniff(std::path::Path::new(&source))
        .with_context(|| format!("Could not read source header: {}", source))?;
    info!(
        "Source image: {} bytes, {:?}",
        std::fs::metadata(&source).map(|m| m.len()).unwrap_or(0),
        caps
    );
    let mode = match requested {
        Some(m) => {
            let risky = libbuf::mode::mode_risky(m, caps);
            info!("Write mode: {} (forced with --mode, mismatched with image: {})", m, risky);
            if risky && !cli.force {
                confirm_risky_mode()?;
            }
            m
        }
        // No --mode given, auto-detect. Hybrids and raw images get dd, extract-only ISOs get copy
        None => {
            let m = libbuf::mode::auto(caps)?;
            info!("Write mode: {} (auto-detected)", m);
            m
        }
    };

    match mode {
        Mode::Dd => write_dd(&cli, &source, &target),
        Mode::Copy => write_copy(&cli, &source, &target, caps),
    }
}

fn log_target_device(target: &str) {
    match libbuf::list_drives() {
        Ok(drives) => match drives.iter().find(|d| d.path.eq_ignore_ascii_case(target)) {
            Some(d) => info!(
                "Target device: {} | {} | {} ({} bytes) | removable={}",
                d.path, d.model, d.size_human, d.size_bytes, d.removable
            ),
            None => info!("Target {} is not in the drive list (image file, or a filtered device)", target),
        },
        Err(e) => info!("Could not enumerate drives to describe the target: {:#}", e),
    }
}

fn write_dd(cli: &Cli, source: &str, target: &str) -> Result<()> {
    if cli.label.is_some() {
        warn!("--label is not usable in dd mode, ignoring");
    }

    let block_size = parse_size(&cli.block_size)
        .map_err(|e| anyhow::anyhow!("Invalid --block-size '{}': {}", cli.block_size, e))?;

    if block_size == 0 {
        bail!("Fatal: Block size must be greater than zero");
    }

    info!("Block size resolved to {} bytes", block_size);

    let params = libbuf::WriteParams {
        source: source.to_string(),
        target: target.to_string(),
        block_size,
        offset: cli.offset,
    };

    say!("\n  Validating source and target...");
    let (source_size, target_file) = libbuf::validate(&params)?;
    say!("  Validation passed.");

    if cli.dry_run {
        say!("\n  --dry-run: all checks passed. Nothing was written.\n");
        return Ok(());
    }

    if !cli.force {
        confirm(source, target, source_size)?;
    } else {
        say!("\n  --force: skipping confirmation, {} -> {}", source, target);
    }

    say!("\n  Writing {} -> {}...\n", source, target);
    libbuf::write(&params, source_size, target_file)?;
    say!("Write completed successfully.");

    Ok(())
}

fn write_copy(cli: &Cli, source: &str, target: &str, caps: libbuf::ImageCaps) -> Result<()> {
    if !caps.copy_capable() {
        bail!(
            "Fatal: copy mode requires an ISO9660 or UDF image, but '{}' is neither. \
             Use --mode dd for raw disk images.",
            source
        );
    }

    let mut irrelevant: Vec<&str> = Vec::new();
    if cli.offset != 0 {
        irrelevant.push("--offset");
    }
    if cli.block_size != "32MiB" {
        irrelevant.push("--block-size");
    }
    if !irrelevant.is_empty() {
        warn!("{} not usable in copy mode, ignoring", irrelevant.join(", "));
    }

    if cli.dry_run {
        return libbuf::copy::run(source, target, true, cli.label.as_deref());
    }

    if !cli.force {
        let iso_len = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
        confirm(source, target, iso_len)?;
    } else {
        say!("\n  --force: skipping confirmation, copy {} -> {}", source, target);
    }

    say!("\n  Copying {} -> {} (ISO mode)...\n", source, target);
    libbuf::copy::run(source, target, false, cli.label.as_deref())
}

fn parse_modes(raw: &[String]) -> Result<Vec<Mode>> {
    let mut out: Vec<Mode> = Vec::new();
    for s in raw {
        let m: Mode = s.parse()?;
        if !out.contains(&m) {
            out.push(m);
        }
    }
    Ok(out)
}

fn validate_label(label: &str) -> Result<()> {
    if label.trim().is_empty() {
        bail!("Fatal: --label cannot be empty");
    }
    if label.chars().count() > 32 {
        bail!("Fatal: --label is limited to 32 characters (NTFS max, FAT32 uses the first 11)");
    }
    if let Some(bad) = label
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == ' '))
    {
        bail!(
            "Fatal: --label contains '{}'; only ASCII letters, digits, spaces, '_' and '-' are allowed",
            bad
        );
    }
    Ok(())
}

fn confirm_risky_mode() -> Result<()> {
    use std::io::{self, Write as _};

    print!(
        "WARNING: This image is not hybrid, and the mode passed may mess with booting, \
         are you sure you want to continue? [y/N]: "
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    info!("Risky mode prompt answered {:?}", input.trim());
    if input.trim().to_ascii_lowercase() != "y" {
        bail!("Aborted by user.");
    }
    Ok(())
}

fn confirm(source: &str, target: &str, source_size: u64) -> Result<()> {
    use libbuf::list::human_bytes;
    use std::io::{self, Write as _};

    print!(
        "\n  Flash {} ({}) to {}? [y/N]: ",
        source,
        human_bytes(source_size),
        target
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_ascii_lowercase();

    if trimmed != "y" {
        info!("User declined (input: {:?})", trimmed);
        bail!("Aborted by user.");
    }

    info!("User confirmed write");
    Ok(())
}

// Accepts plain bytes or suffixes: K/KB/KiB, M/MB/MiB, G/GB/GiB (case-insensitive, powers of 1024)
fn parse_size(s: &str) -> Result<usize> {
    let s = s.trim();
    let split_pos = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(split_pos);

    if num_str.is_empty() {
        bail!("No numeric value found in '{}'", s);
    }

    let num: u64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("Could not parse '{}' as a number", num_str))?;

    let multiplier: u64 = match suffix.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        other => bail!("Unknown size suffix: '{}'", other),
    };

    let bytes = num
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("Size overflows u64: {}", s))?;

    if bytes > usize::MAX as u64 {
        bail!("Block size {} is too large for this platform", s);
    }

    Ok(bytes as usize)
}

fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::BrightCyan.on_default() | Effects::BOLD)
        .usage(AnsiColor::BrightCyan.on_default() | Effects::BOLD)
        .literal(AnsiColor::BrightGreen.on_default())
        .placeholder(AnsiColor::BrightYellow.on_default())
        .error(AnsiColor::BrightRed.on_default() | Effects::BOLD)
        .valid(AnsiColor::BrightGreen.on_default())
        .invalid(AnsiColor::BrightRed.on_default())
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn test_parse_size_plain() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1").unwrap(), 1);
    }

    #[test]
    fn test_parse_size_kib() {
        assert_eq!(parse_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_size("4KB").unwrap(), 4 * 1024);
        assert_eq!(parse_size("4K").unwrap(), 4 * 1024);
        assert_eq!(parse_size("4kib").unwrap(), 4 * 1024);
    }

    #[test]
    fn test_parse_size_mib() {
        assert_eq!(parse_size("32MiB").unwrap(), 32 * 1024 * 1024);
        assert_eq!(parse_size("32MB").unwrap(), 32 * 1024 * 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
    }

    #[test]
    fn test_parse_size_gib() {
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_bad_suffix() {
        assert!(parse_size("1TiB").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("MiB").is_err());
    }
}
