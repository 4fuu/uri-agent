#!/usr/bin/env python3
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "https://github.com/4fuu/uri-agent"
TARGETS = {
    "linux_arm": ("aarch64-unknown-linux-gnu", "tar.gz"),
    "linux_intel": ("x86_64-unknown-linux-gnu", "tar.gz"),
    "mac_arm": ("aarch64-apple-darwin", "tar.gz"),
    "mac_intel": ("x86_64-apple-darwin", "tar.gz"),
    "windows_intel": ("x86_64-pc-windows-msvc", "zip"),
}


def asset_name(version: str, target: str, extension: str) -> str:
    return f"uri-agent-{version}-{target}.{extension}"


def read_checksums(version: str, path: Path) -> dict[str, str]:
    checksums: dict[str, str] = {}
    for line in path.read_text().splitlines():
        fields = line.split()
        if len(fields) != 2 or not re.fullmatch(r"[0-9a-fA-F]{64}", fields[0]):
            raise SystemExit(f"invalid checksum line: {line!r}")
        filename = fields[1].removeprefix("*")
        if filename in checksums:
            raise SystemExit(f"duplicate checksum for {filename}")
        checksums[filename] = fields[0].lower()

    expected = {
        asset_name(version, target, extension)
        for target, extension in TARGETS.values()
    }
    if checksums.keys() != expected:
        missing = sorted(expected - checksums.keys())
        extra = sorted(checksums.keys() - expected)
        raise SystemExit(f"checksum assets do not match release matrix; missing={missing}, extra={extra}")
    return checksums


def release_url(version: str, target: str, extension: str) -> str:
    asset = asset_name(version, target, extension)
    return f"{REPOSITORY}/releases/download/v{version}/{asset}"


def formula(version: str, checksums: dict[str, str]) -> str:
    def source(key: str) -> tuple[str, str]:
        target, extension = TARGETS[key]
        asset = asset_name(version, target, extension)
        return release_url(version, target, extension), checksums[asset]

    mac_arm_url, mac_arm_sha = source("mac_arm")
    mac_intel_url, mac_intel_sha = source("mac_intel")
    linux_arm_url, linux_arm_sha = source("linux_arm")
    linux_intel_url, linux_intel_sha = source("linux_intel")
    return f'''class UriAgent < Formula
  desc "Protocol-oriented coding agent with a focused terminal interface"
  homepage "{REPOSITORY}"
  version "{version}"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "{mac_arm_url}"
      sha256 "{mac_arm_sha}"
    else
      url "{mac_intel_url}"
      sha256 "{mac_intel_sha}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "{linux_arm_url}"
      sha256 "{linux_arm_sha}"
    else
      url "{linux_intel_url}"
      sha256 "{linux_intel_sha}"
    end
  end

  def install
    bin.install "uri-agent"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/uri-agent --version")
  end
end
'''


def scoop_manifest(version: str, checksums: dict[str, str]) -> dict[str, object]:
    target, extension = TARGETS["windows_intel"]
    asset = asset_name(version, target, extension)
    return {
        "version": version,
        "description": "Protocol-oriented coding agent with a focused terminal interface",
        "homepage": REPOSITORY,
        "license": "MIT",
        "architecture": {
            "64bit": {
                "url": release_url(version, target, extension),
                "hash": checksums[asset],
            }
        },
        "bin": "uri-agent.exe",
        "checkver": {"github": REPOSITORY},
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": f"{REPOSITORY}/releases/download/v$version/uri-agent-$version-{target}.zip"
                }
            }
        },
    }


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(
            f"usage: {Path(sys.argv[0]).name} <version> <SHA256SUMS>"
        )
    version = sys.argv[1]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise SystemExit("version must contain three numeric components")
    checksums = read_checksums(version, Path(sys.argv[2]))

    (ROOT / "Formula").mkdir(exist_ok=True)
    (ROOT / "bucket").mkdir(exist_ok=True)
    (ROOT / "Formula/uri-agent.rb").write_text(formula(version, checksums))
    (ROOT / "bucket/uri-agent.json").write_text(
        json.dumps(scoop_manifest(version, checksums), indent=4) + "\n"
    )


if __name__ == "__main__":
    main()
