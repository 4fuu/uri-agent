class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.903.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.903.0/uri-agent-2026.903.0-aarch64-apple-darwin.tar.gz"
    sha256 "60e03c602a12f559d4490dad876fb270600c665820a91cf6af26b4e560d28a71"
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
