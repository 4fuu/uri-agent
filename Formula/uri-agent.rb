class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.830.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.830.0/uri-agent-2026.830.0-aarch64-apple-darwin.tar.gz"
    sha256 "7e60672afa80669ad4e198e862f5bcd925cf6524810651a6c8b525e28e0c7226"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
