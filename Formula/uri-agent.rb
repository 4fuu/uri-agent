class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.828.1"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.828.1/uri-agent-2026.828.1-aarch64-apple-darwin.tar.gz"
    sha256 "aecaa9a1a4acc108e5f5d5cd194d27ca1c22a62d371ce373f1d07ce5e2eb1a3d"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
