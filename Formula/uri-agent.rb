class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.915.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.915.0/uri-agent-2026.915.0-aarch64-apple-darwin.tar.gz"
    sha256 "d6e6818ae0d5e8242ebf900c396c40e880d3421c7214c72001cd1e09851943f6"
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
