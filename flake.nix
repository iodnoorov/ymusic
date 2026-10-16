{
  description = "ymusic-cli — Yandex Music in the terminal";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        ymusic-cli = pkgs.callPackage ./nix/package.nix { };
        default = ymusic-cli;
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/ymusic";
        };
      });

      overlays.default = final: _prev: {
        ymusic-cli = final.callPackage ./nix/package.nix { };
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            mpv
          ];
        };
      });
    };
}
