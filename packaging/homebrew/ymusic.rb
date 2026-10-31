# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.7.0/ymusic-3.7.0-aarch64-macos.tar.gz"
      sha256 "52009ad6e27334b766e0596748615874a2ede26df3d26d0ed6a274c3a087ae9b"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.7.0/ymusic-3.7.0-x86_64-macos.tar.gz"
      sha256 "fbe43e56ceede4befcbbaa05c6699d1e3eca4f490e33b06aed57d8573a1df2fe"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.7.0/ymusic-3.7.0-aarch64-linux.tar.gz"
      sha256 "db3179fe660a29621826f347bae5eae7b376e50ee8b4d4383c109de9414a8001"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.7.0/ymusic-3.7.0-x86_64-linux.tar.gz"
      sha256 "3f53c0ff8ab1c5d82d1887cde6abac1e7a3cadc15b3a86b4fb22015937609f16"
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
