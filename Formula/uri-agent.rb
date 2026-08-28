class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.828.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.828.0/uri-agent-2026.828.0-aarch64-apple-darwin.tar.gz"
    sha256 "6bca56e9965c3bd3887714e98cbd033add17bd097b9f1066b5058440dfa23381"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
