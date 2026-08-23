class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.823.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.823.0/uri-agent-2026.823.0-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.823.0/uri-agent-2026.823.0-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.823.0/uri-agent-2026.823.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.823.0/uri-agent-2026.823.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
