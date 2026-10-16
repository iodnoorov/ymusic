{
  lib,
  rustPlatform,
  makeWrapper,
  mpv,
}:

rustPlatform.buildRustPackage {
  pname = "ymusic-cli";
  version = "0.5.0";

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
    wrapProgram $out/bin/ymusic --prefix PATH : ${lib.makeBinPath [ mpv ]}
  '';

  meta = {
    description = "Terminal client for Yandex Music: My Wave, likes, stations (TUI)";
    homepage = "https://github.com/iodnoorov/yamusic-cli";
    license = lib.licenses.mit;
    mainProgram = "ymusic";
    platforms = lib.platforms.linux;
  };
}
