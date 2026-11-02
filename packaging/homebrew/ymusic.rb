# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.9.0/ymusic-3.9.0-aarch64-macos.tar.gz"
      sha256 "8457013f5e535e2db71a46eb865187da7bf6e13378dd1768efe571e0f8fc7613"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.9.0/ymusic-3.9.0-x86_64-macos.tar.gz"
      sha256 "c1dbf8a6c0c652fdab8f01a4fc871b1b0fd822a9a61b444d9927777c36207a29"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.9.0/ymusic-3.9.0-aarch64-linux.tar.gz"
      sha256 "eeea079c5c233f467e0c82797714016a0dfa38d34b0d83d7989a9d92bf03366a"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.9.0/ymusic-3.9.0-x86_64-linux.tar.gz"
      sha256 "30cf6619aa68abe87f39b890546049a7e40ae9eb5fb6424125e97a58054eeb5c"
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
