class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.829.1"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.829.1/uri-agent-2026.829.1-aarch64-apple-darwin.tar.gz"
    sha256 "61d1700b1af6f5e0d2f1ea38af7cf34f15d7e4e16cdac1b8e123a6dd9d92623a"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
