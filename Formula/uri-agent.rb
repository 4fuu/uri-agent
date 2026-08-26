class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.826.1"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.826.1/uri-agent-2026.826.1-aarch64-apple-darwin.tar.gz"
    sha256 "1aaf41a658436c2f0f3e660fa4c55362e01cd15c6e2c1c620bcae27e7a749f71"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
