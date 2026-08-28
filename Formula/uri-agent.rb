class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.829.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.829.0/uri-agent-2026.829.0-aarch64-apple-darwin.tar.gz"
    sha256 "3d8dd39a29a426c8c0c4eef53000c76b5c8133f1a54074647356f327eb23f795"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
