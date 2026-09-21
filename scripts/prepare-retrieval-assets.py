#!/usr/bin/env python3
"""Download and verify the fixed native/search assets used in release archives."""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import tarfile
import tempfile
import urllib.request
from pathlib import Path

ZVEC_VERSION = "0.7.2"
ZVEC_ARCHIVES = {
    "x86_64-unknown-linux-gnu": "123bde64ed8baae5ea813907af5ab3575113281eb81fca5310f28a0fe4ffb92b",
    "aarch64-unknown-linux-gnu": "7c8add821bc247ecdb7a688d456ae60f4dca91078e942084300757bf2d9d91fb",
    "aarch64-apple-darwin": "5fd12d7659b495bf6b06ba173876e400dae1dc19d5e04e8468381c82867ea47f",
    "x86_64-pc-windows-msvc": "13e416efbe72730329a61a74eccb1e8c32c7dbd203bcd1dc7c325ee52b8cfc0a",
}
ZVEC_FILES = {
    "x86_64-unknown-linux-gnu": (
        ("libzvec_c_api.so", "58381ac7b12afd5eeae3dc10325914a28fc3157061291bb693a9ed757d815b8a"),
    ),
    "aarch64-unknown-linux-gnu": (
        ("libzvec_c_api.so", "abadfd17a6e8aa648d77dc48816cfc965da5de3cf5ac2f42db15c1a89e8351b5"),
    ),
    "aarch64-apple-darwin": (
        ("libzvec_c_api.dylib", "06f183bf51ebd8b460fd0415f6b39c2556bdd6ef63a270efa35bdf76154512e2"),
    ),
    "x86_64-pc-windows-msvc": (
        ("zvec_c_api.dll", "d846eeb9e84a1409bdd38723b1275bb2968468d98ef04fdb8f0b26682f17382e"),
        ("zvec_c_api.lib", "404d08fc55680a1bbc4351041826d5b643ebeb1767ad19931cb9e077aa24f7f7"),
    ),
}

JIEBA_COMMIT = "b3602bef7d1f67521a61788a74fb5801a0e62cd3"
MODEL_REVISION = "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b"
FILES = (
    ("retrieval/jieba/jieba.dict.utf8", f"https://raw.githubusercontent.com/yanyiwu/cppjieba/{JIEBA_COMMIT}/dict/jieba.dict.utf8", "6f7d4350e8861ef4139b2e3a6fad05430c19ae71f4b8378190edecac8aae2e6a"),
    ("retrieval/jieba/hmm_model.utf8", f"https://raw.githubusercontent.com/yanyiwu/cppjieba/{JIEBA_COMMIT}/dict/hmm_model.utf8", "f17790586ac86dd048c8adffed052c4bd2b28ed0682972c1275e59040c0589a7"),
    ("retrieval/jieba/LICENSE", f"https://raw.githubusercontent.com/yanyiwu/cppjieba/{JIEBA_COMMIT}/LICENSE", "ba898a14f729ba5e9965da34e3eecd5edd3795f2cc5d7c923b815ba79bb851b0"),
    ("retrieval/models/potion-code-16M-v2/model.safetensors", f"https://huggingface.co/minishlab/potion-code-16M-v2/resolve/{MODEL_REVISION}/model.safetensors?download=true", "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c"),
    ("retrieval/models/potion-code-16M-v2/tokenizer.json", f"https://huggingface.co/minishlab/potion-code-16M-v2/resolve/{MODEL_REVISION}/tokenizer.json?download=true", "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45"),
    # The model revision declares MIT but contains no separate LICENSE file.
    ("retrieval/models/potion-code-16M-v2/LICENSE", "https://raw.githubusercontent.com/MinishLab/model2vec/f16a2cee72e4ba9637f4b5ca31774658f1f292c3/LICENSE", "b0214f148eceae739f916209e78cfe15712c4b810c8bc9d44b6c2d1d1616aab6"),
    ("retrieval/licenses/zvec-LICENSE", f"https://raw.githubusercontent.com/zvec-ai/zvec-rust/v{ZVEC_VERSION}/LICENSE", "43070e2d4e532684de521b885f385d0841030efa2b1a20bafb76133a5e1379c1"),
    # zvec-rust's v0.7.2 submodule points at this exact zvec core commit.
    ("retrieval/licenses/zvec-NOTICE", "https://raw.githubusercontent.com/alibaba/zvec/1ab7975dfc2d2160054bafff614831b7099cd930/NOTICE", "332b1a498b446fab1232b671c2ba74102fc563c198dc6f53980d1282075958ad"),
)


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def download(url: str, destination: Path, expected: str) -> None:
    if destination.is_file() and digest(destination) == expected:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.download-{os.getpid()}")
    try:
        with urllib.request.urlopen(url) as response, temporary.open("wb") as output:
            shutil.copyfileobj(response, output)
        actual = digest(temporary)
        if actual != expected:
            raise SystemExit(f"SHA256 mismatch for {url}: expected {expected}, got {actual}")
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def extract_zvec(archive: Path, target: str, output: Path) -> None:
    expected = {name for name, _ in ZVEC_FILES[target]}
    found: dict[str, tarfile.TarInfo] = {}
    with tarfile.open(archive, "r:gz") as bundle:
        for member in bundle.getmembers():
            normalized = member.name.removeprefix("./")
            if member.issym() or member.islnk() or normalized.startswith("/") or ".." in Path(normalized).parts:
                raise SystemExit(f"unsafe member in zvec archive: {member.name}")
            if normalized in expected and member.isfile():
                found[normalized] = member
        if found.keys() != expected:
            raise SystemExit(f"zvec archive files do not match {target}: {sorted(found)}")
        for name, member in found.items():
            source = bundle.extractfile(member)
            if source is None:
                raise SystemExit(f"could not read {member.name}")
            destination = output / name
            temporary = destination.with_name(f".{name}.extract-{os.getpid()}")
            with source, temporary.open("wb") as result:
                shutil.copyfileobj(source, result)
            os.replace(temporary, destination)


def verify(target: str, output: Path) -> None:
    missing = [path for path, _, checksum in FILES if not (output / path).is_file() or digest(output / path) != checksum]
    missing += [
        name
        for name, checksum in ZVEC_FILES[target]
        if not (output / name).is_file() or digest(output / name) != checksum
    ]
    if missing:
        raise SystemExit(f"missing or invalid retrieval assets: {', '.join(missing)}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=sorted(ZVEC_ARCHIVES))
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--verify-only", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    if not args.verify_only:
        with tempfile.TemporaryDirectory(prefix="uri-agent-assets-") as temporary:
            archive = Path(temporary) / "zvec.tar.gz"
            download(f"https://github.com/zvec-ai/zvec-rust/releases/download/v{ZVEC_VERSION}/zvec-prebuilt-{args.target}.tar.gz", archive, ZVEC_ARCHIVES[args.target])
            extract_zvec(archive, args.target, args.output)
        for path, url, checksum in FILES:
            download(url, args.output / path, checksum)
    verify(args.target, args.output)


if __name__ == "__main__":
    main()
