# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.5.1/ymusic-3.5.1-aarch64-macos.tar.gz"
      sha256 "SHA256_AARCH64_MACOS"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.5.1/ymusic-3.5.1-x86_64-macos.tar.gz"
      sha256 "SHA256_X86_64_MACOS"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.5.1/ymusic-3.5.1-aarch64-linux.tar.gz"
      sha256 "SHA256_AARCH64_LINUX"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.5.1/ymusic-3.5.1-x86_64-linux.tar.gz"
      sha256 "SHA256_X86_64_LINUX"
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
