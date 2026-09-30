# Downloading

PREREQUISITES (non-Windows):
- [git](https://git-scm.com/install)
- [Rust](https://rust-lang.org/tools/install/)

## Linux and macOS

**Arch Users**:
bufusb is available on the AUR.
```bash
yay -S bufusb-cli
```

**Other Distributions and macOS**:
```bash
curl -fsSL https://raw.githubusercontent.com/brysonak/bufusb/refs/heads/main/Install/install.sh | sh
```
**NOTE**: This script will ask for privileges, and needs a C compiler (`gcc`/`clang`) on PATH to link the rust binary. **If you're on NixOS**, use the flake instead, see below.

## NixOS

NixOS users can install straight from the flake.
```bash
nix profile install github:brysonak/bufusb
```

## Windows

If you're on x64, download the `bufusb-setup-windows-x86_64.zip` file and run the installer inside it. [releases page](https://github.com/brysonak/bufusb/releases).

If you're on ARM64 hardware, download `bufusb-setup-windows-aarch64.zip` instead and run the installer inside that.
