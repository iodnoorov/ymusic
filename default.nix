# Non-flake usage: nix-build  /  pkgs.callPackage ./path/to/ymusic { }
{
  pkgs ? import <nixpkgs> { },
}:
pkgs.callPackage ./nix/package.nix { }
