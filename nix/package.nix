{
  lib,
  rustPlatform,
  makeWrapper,
  mpv,
}:

rustPlatform.buildRustPackage {
  pname = "yamusic-cli";
  version = "0.2.0";

  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  # mpv is looked up in PATH at runtime
  postInstall = ''
    wrapProgram $out/bin/yamusic --prefix PATH : ${lib.makeBinPath [ mpv ]}
  '';

  meta = {
    description = "Terminal client for Yandex Music: My Wave, likes, stations (TUI)";
    homepage = "https://github.com/rexilone/yamusic-cli";
    license = lib.licenses.mit;
    mainProgram = "yamusic";
    platforms = lib.platforms.linux;
  };
}
