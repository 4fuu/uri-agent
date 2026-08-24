class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.824.1"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.824.1/uri-agent-2026.824.1-aarch64-apple-darwin.tar.gz"
      sha256 "d54028341f7cadb0496fa7ecf196d775aaca4b75b8c5f591b7daffe30046a843"
    else
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.824.1/uri-agent-2026.824.1-x86_64-apple-darwin.tar.gz"
      sha256 "5d7b3ec037f9a9707569bc7ae47a483377663f30c9eeeb1d151ced1355622f89"
    end
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
