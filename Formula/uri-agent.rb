class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.902.1"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.902.1/uri-agent-2026.902.1-aarch64-apple-darwin.tar.gz"
    sha256 "3ed419bf803b569211718a414f743e7becbe57384779a22cc10d727801de2d73"
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
