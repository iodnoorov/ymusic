# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.10.0/ymusic-3.10.0-aarch64-macos.tar.gz"
      sha256 "ef6818d3b96e7125aacd19d51a48bed9df32e9e1a1fb9aeff42b6ed5e8905c33"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.10.0/ymusic-3.10.0-x86_64-macos.tar.gz"
      sha256 "ed237b86f8b47eea05fec5267c759272f5404b9ece272d5182964ba17a9d013b"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.10.0/ymusic-3.10.0-aarch64-linux.tar.gz"
      sha256 "f8fd968dac0890d6835f88501a0f0e0882bd511f9ac64d6b0622ce7182013bbc"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.10.0/ymusic-3.10.0-x86_64-linux.tar.gz"
      sha256 "5b1165e865042588dd08225080cafbb158247259c65818fc666c1743ecbfd68b"
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
