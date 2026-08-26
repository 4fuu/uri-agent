class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.827.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.827.0/uri-agent-2026.827.0-aarch64-apple-darwin.tar.gz"
    sha256 "fb6d7a5ddfb4a8eef07b60c581f80d0655bcf4236c72fb7ef5f0d6a37d2905f8"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
