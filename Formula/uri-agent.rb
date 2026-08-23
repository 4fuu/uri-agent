class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "https://github.com/4fuu/uri-agent"
  version "2026.824.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.824.0/uri-agent-2026.824.0-aarch64-apple-darwin.tar.gz"
      sha256 "3ac016e8229218ceb09e72741cb344d8ce1f8ca03ca5530a9b9f6fc708b85a02"
    else
      url "https://github.com/4fuu/uri-agent/releases/download/v2026.824.0/uri-agent-2026.824.0-x86_64-apple-darwin.tar.gz"
      sha256 "d477d59f4770f0595be28103345277f798df8a27e2807ca57996432de9eaf610"
    end
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/uri-agent --version")
  end
end
