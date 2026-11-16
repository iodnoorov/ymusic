# Formula of the tap iodnoorov/homebrew-tap (Formula/ymusic.rb there): the release binaries.
# On a release: the version in the four URLs, then the sha256 of each archive from its .sha256 file.
class Ymusic < Formula
  desc "Terminal client for Yandex Music"
  homepage "https://github.com/iodnoorov/ymusic"
  license all_of: ["MIT", "FDK-AAC"]

  on_macos do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.11.0/ymusic-3.11.0-aarch64-macos.tar.gz"
      sha256 "f2ca16243c6258e16df5b997a6a6d91aa9d234e908b14afae4443c325a5f0670"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.11.0/ymusic-3.11.0-x86_64-macos.tar.gz"
      sha256 "0ff315c0944d6b62f3306d67505e2624743db9d32484254b674942c87f197d45"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.11.0/ymusic-3.11.0-aarch64-linux.tar.gz"
      sha256 "6ec3b36501b6ff7135a45ff95759db9b3d06d8ae0dc814c750eb72b2f98d205c"
    end
    on_intel do
      url "https://github.com/iodnoorov/ymusic/releases/download/v3.11.0/ymusic-3.11.0-x86_64-linux.tar.gz"
      sha256 "e1055f6ab5444b6e85ca8c36816169109f07d074c56bfd3e2e1b7e77fc6ec39b"
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
