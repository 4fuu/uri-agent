class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.826.2"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.826.2/uri-agent-2026.826.2-aarch64-apple-darwin.tar.gz"
    sha256 "ee5399506b59216cb1e9c18b6d57b5fc124bc0dbb12ec252442f57af1bc896df"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
