# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.8.0/ymusic-3.8.0-aarch64-macos.tar.gz"
      sha256 "f09adb8b91f0ad9a5a1404d1730854b6bd3e98d6bd36cd0e3a4bdf68b6eac049"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.8.0/ymusic-3.8.0-x86_64-macos.tar.gz"
      sha256 "635fca9b9de3a75fdfaea53a9f63cb47ce54921c57a23fd481afcb96770829b4"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.8.0/ymusic-3.8.0-aarch64-linux.tar.gz"
      sha256 "1d0daaedc06393a8b6c8df059bb38726e267966118a05246c606cf86b6f946a6"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.8.0/ymusic-3.8.0-x86_64-linux.tar.gz"
      sha256 "a2de15503c5e8ea53bc93ae51ce33a371acc442dc8a1538224ff67f861cd575b"
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
