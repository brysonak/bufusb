# Downloading

PREREQUISITES (non-Windows):
- [git](https://git-scm.com/install)
- [Rust](https://rust-lang.org/tools/install/)

## Linux and macOS

**THE PACKAGE ON THE AUR IS IN THE MIDDLE OF BEING RENAMED. Please standby**

**Linux and macOS**:
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

Run the `bufusb-setup.exe` installer from the [releases page](https://github.com/brysonak/bufusb/releases).
