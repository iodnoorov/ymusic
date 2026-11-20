# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.14.0/ymusic-3.14.0-aarch64-macos.tar.gz"
      sha256 "464bab8e0143abe49acbf01c1e7af15bc2ef6eb096e0d32a09f745d4ffce9b72"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.14.0/ymusic-3.14.0-x86_64-macos.tar.gz"
      sha256 "ea69b66023bbd41a026c8d834071d5311a87e3ec26d68d440a77b202d627c7dc"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.14.0/ymusic-3.14.0-aarch64-linux.tar.gz"
      sha256 "fac3d0b58b4321381b9d72439f8e7ace48154ce510a99f0969790df425388f19"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.14.0/ymusic-3.14.0-x86_64-linux.tar.gz"
      sha256 "849a66e73d2032f644616591719c7963272d9c13592df781d80db01f81091675"
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
