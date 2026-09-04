class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.904.3"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.904.3/uri-agent-2026.904.3-aarch64-apple-darwin.tar.gz"
    sha256 "768a51b6b51ee3bd8d79b003ebc76f278c28e21f0f3c4d1a5462dbcfa94bf1f9"
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
