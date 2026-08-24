class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.824.3"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.824.3/uri-agent-2026.824.3-aarch64-apple-darwin.tar.gz"
    sha256 "d22e802a6231fb1ea5412cc78de016f64957b8045adaa06215f671b2f17dd200"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
