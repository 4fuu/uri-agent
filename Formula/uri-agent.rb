class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.826.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.826.0/uri-agent-2026.826.0-aarch64-apple-darwin.tar.gz"
    sha256 "eaec73e3aa06204ac3855b309f7406010344a687538e5cb3df9118729127fe17"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
