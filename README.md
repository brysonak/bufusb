# bufusb
![buf logo](buf.png)
**B**ootable **U**SB **F**lasher is a tool made for flashing .iso/.img files onto USB drives, for booting into operating systems of course...
 Is it bootable USB flasher or Bryson's USB flasher?

bufusb is fully cross-platform and open source, under [GPL-v3](https://www.gnu.org/licenses/gpl-3.0.html).

Logo made by [Mia](https://github.com/marshmallow-mia)

# Installation 
Instructions have moved to [downloading.md](docs/downloading.md)


# Building
Pre-requisites:
- [git](https://git-scm.com/install)
- [Rust](https://rust-lang.org/tools/install/)

Run the following commands:
```bash
git clone https://github.com/brysonak/bufusb.git

cd bufusb

cargo build --release
```
This will produce a binary inside `target/release`.

To use it straight from the CLI, it's recommended that you add it to PATH on windows. If you're on linux, copy the binary into /usr/bin (or /usr/local/bin) then restart any open shell to use it.