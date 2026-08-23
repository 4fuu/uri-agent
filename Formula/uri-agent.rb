class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.823.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.823.0/uri-agent-2026.823.0-aarch64-apple-darwin.tar.gz"
      sha256 "21a9c5267816e9e911e937b001d6d45f14c5a8a83ae7016f26f1c637f3b72e7e"
    else
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.823.0/uri-agent-2026.823.0-x86_64-apple-darwin.tar.gz"
      sha256 "f8c8cf2e0453854b4701d5eb22e1feed57e866cb7bcb991077d93f2646e3d980"
    end
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
