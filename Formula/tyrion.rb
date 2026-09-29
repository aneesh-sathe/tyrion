# Tap this repository directly, then install:
#
#   brew tap aneesh-sathe/tyrion https://github.com/aneesh-sathe/tyrion
#   brew install tyrion
class Tyrion < Formula
  desc "Runs coding agents in parallel under containment and accepts only verified work"
  homepage "https://github.com/aneesh-sathe/tyrion"
  url "https://github.com/aneesh-sathe/tyrion/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "58cfb1c5252ba36596298bf2c9e2c65e3ae45c92c256bca28d59d924a63366c0"
  license "MIT"
  head "https://github.com/aneesh-sathe/tyrion.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      Tyrion runs every agent inside Docker. With Docker Desktop or Colima
      running, finish setup with:

        tyrion init
    EOS
  end

  test do
    assert_match "tyrion", shell_output("#{bin}/tyrion --version")
  end
end
