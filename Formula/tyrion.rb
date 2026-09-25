# Tap this repository directly, then install:
#
#   brew tap aneesh-sathe/tyrion https://github.com/aneesh-sathe/tyrion
#   brew install tyrion
class Tyrion < Formula
  desc "Runs coding agents in parallel under containment and accepts only verified work"
  homepage "https://github.com/aneesh-sathe/tyrion"
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
