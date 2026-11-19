# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.13.0/ymusic-3.13.0-aarch64-macos.tar.gz"
      sha256 "0ea261b96c8728a9fefc7ceb206714923ef5573dccb4ea4692b3fad0d9c8f4d6"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.13.0/ymusic-3.13.0-x86_64-macos.tar.gz"
      sha256 "82e6e34fdb55bd921462355a20c39754b52c10034385d5c0ab991b47a9e9e628"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.13.0/ymusic-3.13.0-aarch64-linux.tar.gz"
      sha256 "6f8243e5931fa1239593d4395f68b39dda39106229add32b3d9e24f6052d5b1b"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.13.0/ymusic-3.13.0-x86_64-linux.tar.gz"
      sha256 "bbb6616afa181083c64071cc91906f5f3b7cbd14329069bd13db584d2ab135d7"
    end
  end

  def install
    bin.install "ymusic"
    doc.install "README.md"
    prefix.install "LICENSE", "LICENSE-FDK-AAC"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ymusic --version")
  end
end
