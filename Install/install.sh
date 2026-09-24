#!/bin/sh
set -eu
REPO="https://github.com/brysonak/bufusb.git"
BIN="bufusb"
PREFIX="${PREFIX:-/usr/local}"
INSTALL_DIR="$PREFIX/bin"
require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'Error: required command "%s" not found.\n' "$1" >&2
        exit 1
    fi
}
case "$(uname -s)" in
    Linux|Darwin)
        ;;
    *)
        printf 'Error: unsupported operating system.\n' >&2
        exit 1
        ;;
esac
if [ -f /etc/NIXOS ]; then
    printf 'NixOS detected, use the flake instead:\n' >&2
    printf '  nix profile install github:brysonak/bufusb\n' >&2
    exit 1
fi
require git
require cargo
require cc
CLONE_DIR=$(mktemp -d)
trap 'rm -rf "$CLONE_DIR"' EXIT
printf 'Cloning %s...\n' "$REPO"
git clone --depth 1 "$REPO" "$CLONE_DIR"
cd "$CLONE_DIR"
cargo build --release --package bufusb
if [ ! -f "target/release/$BIN" ]; then
    printf 'Error: build failed, target/release/%s not found.\n' "$BIN" >&2
    exit 1
fi
find_privilege_command() {
    if [ "$(id -u)" -eq 0 ]; then
        return 0
    fi
    for cmd in doas sudo pkexec; do
        if command -v "$cmd" >/dev/null 2>&1; then
            PRIVCMD="$cmd"
            return 0
        fi
    done
    return 1
}
run_privileged() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
        return
    fi
    if [ -n "${PRIVCMD:-}" ]; then
        "$PRIVCMD" "$@"
        return
    fi
    printf 'Error: Insufficient Permissions\n' >&2
    printf 'Install sudo, doas or pkexec, or set PREFIX to a writable directory.\n' >&2
    exit 1
}
mkdir -p "$INSTALL_DIR" 2>/dev/null || true
if [ -w "$INSTALL_DIR" ]; then
    install -m755 "target/release/$BIN" "$INSTALL_DIR/$BIN"
else
    find_privilege_command || {
        printf 'Error: cannot write to %s.\n' "$INSTALL_DIR" >&2
        exit 1
    }
    run_privileged install -m755 "target/release/$BIN" "$INSTALL_DIR/$BIN"
fi
printf '\nbuf installed successfully.\n'
printf 'Executable: %s/%s\n' "$INSTALL_DIR" "$BIN"
if command -v "$INSTALL_DIR/$BIN" >/dev/null 2>&1; then
    "$INSTALL_DIR/$BIN" --version || true
else
    printf '\nNote: %s may not be in your PATH.\n' "$INSTALL_DIR"
    printf "Restart your shell or run 'hash -r'.\n"
fi