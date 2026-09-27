class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.927.1"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.927.1/uri-agent-2026.927.1-aarch64-apple-darwin.tar.gz"
    sha256 "d2a968757e52b52f1a9111188e922dba27426f4f21e195b18b9c75b965e48560"
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
