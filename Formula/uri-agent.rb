class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.824.5"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.824.5/uri-agent-2026.824.5-aarch64-apple-darwin.tar.gz"
    sha256 "32d7b6bc2436f49c42a3714d6166548e9118b5ffe0fa8c89ea5888c7b3ee9bf2"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
