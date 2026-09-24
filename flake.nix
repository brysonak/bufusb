{
  description = "bufusb, bootable USB flasher";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "bufusb";
          version = "0.2.3";
          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          cargoBuildFlags = [ "--package" "bufusb" ];
          doCheck = false;

          meta = with pkgs.lib; {
            description = "A fast, safe bootable USB image flasher";
            homepage = "https://github.com/brysonak/bufusb";
            license = licenses.gpl3Plus;
            mainProgram = "bufusb";
          };
        };

        apps.default = flake-utils.lib.mkApp { drv = self.packages.${system}.default; };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [ cargo-watch clippy ntfs3g ];
        };
      });
}