class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.825.0"
  license "MIT"
  depends_on arch: :arm64

  on_macos do
    url "https://github.com/4fuu/uri-agent/releases/download/v2026.825.0/uri-agent-2026.825.0-aarch64-apple-darwin.tar.gz"
    sha256 "6339cca6950a756588d21e5c6ad54e4f2e41b6aec4448a50b2edf36ccbcc5c72"
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
