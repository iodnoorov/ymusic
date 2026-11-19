# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.12.0/ymusic-3.12.0-aarch64-macos.tar.gz"
      sha256 "0889c7ec8b49909755bd35e7ac8ca843362889d03101b080f8ae0e0c4a3f06b7"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.12.0/ymusic-3.12.0-x86_64-macos.tar.gz"
      sha256 "b751e0105a3bd66160ac3ebdbb2f62a7e0aa6f9f624f2c1d838747f2fec30ac5"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.12.0/ymusic-3.12.0-aarch64-linux.tar.gz"
      sha256 "e9005477271ccb2f47d45a5999dab60958375a109eaa1b588a3760191d78295c"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.12.0/ymusic-3.12.0-x86_64-linux.tar.gz"
      sha256 "607c604035a5347c76af3e3943b8ccdbdc29ba4795ba1c2905d1e4df727ab84f"
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
