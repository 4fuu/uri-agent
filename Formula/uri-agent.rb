class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.827.1"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.827.1/uri-agent-2026.827.1-aarch64-apple-darwin.tar.gz"
    sha256 "ec91c8229fc8a0c264d9c5016605c6090ffa00e97710fe41ef264f9380aa3571"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
