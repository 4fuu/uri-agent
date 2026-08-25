#!/usr/bin/env python3
from __future__ import annotations

import re
import subprocess
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION_PATTERN = re.compile(r"^(\d{4})\.(\d{3,4})\.(\d+)$")


def validate_version(version: str) -> None:
    match = VERSION_PATTERN.fullmatch(version)
    if not match:
        raise SystemExit("version must use YYYY.MDD.REVISION, for example 2026.823.0")
    year, month_day, _ = match.groups()
    month = int(month_day[:-2])
    day = int(month_day[-2:])
    try:
        parsed = date(int(year), month, day)
    except ValueError as error:
        raise SystemExit(f"invalid calendar version {version}: {error}") from error
    if month_day != f"{parsed.month}{parsed.day:02d}":
        raise SystemExit("month must be unpadded and day must be two digits")


def replace_once(path: str, pattern: str, replacement: str) -> None:
    file = ROOT / path
    content = file.read_text()
    updated, count = re.subn(pattern, replacement, content, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(f"expected one version field in {path}, found {count}")
    file.write_text(updated)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} <YYYY.MDD.REVISION>")
    version = sys.argv[1]
    validate_version(version)

    replace_once("Cargo.toml", r'^version = "\d+\.\d+\.\d+"$', f'version = "{version}"')
    replace_once(
        "Cargo.toml",
        r'^(uri-agent-plugin-sdk = \{ version = ")=\d+\.\d+\.\d+(".*)$',
        rf'\g<1>={version}\g<2>',
    )
    replace_once(
        "sdk/Cargo.toml",
        r'^version = "\d+\.\d+\.\d+"$',
        f'version = "{version}"',
    )
    replace_once(
        "sdk/README.md",
        r'^(uri-agent-plugin-sdk = ")\d+\.\d+\.\d+("$)',
        rf'\g<1>{version}\g<2>',
    )
    for package in ("uri-agent", "uri-agent-plugin-sdk"):
        replace_once(
            "Cargo.lock",
            rf'^(name = "{package}"\nversion = ")\d+\.\d+\.\d+("$)',
            rf'\g<1>{version}\g<2>',
        )

    subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        check=True,
    )
    print(f"Set URI Agent and plugin SDK versions to {version}")


if __name__ == "__main__":
    main()
