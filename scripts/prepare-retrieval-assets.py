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

ZVEC_VERSION = "0.7.0"
ZVEC_ARCHIVES = {
    "x86_64-unknown-linux-gnu": "7e9adbeadc42c772665efed45112220aa895d3f7963fa03c016102f2f414c37f",
    "aarch64-unknown-linux-gnu": "0195a85f07370d7bcbf26f990bf794e31430e00224c8b9303d43ea677db6f77d",
    "aarch64-apple-darwin": "59c41dcbaab69b9fbcf3ca0f1997f58f189a025657fd09a464dca199107cdeb2",
    "x86_64-pc-windows-msvc": "d8fe5585ad83066038f6e60990fe6e69528637a58fffc5024ca211c187a9d49a",
}
ZVEC_FILES = {
    "x86_64-unknown-linux-gnu": (
        ("libzvec_c_api.so", "89eac719eb426a2066d2104e5b1199aa83ec18eaa4c31c7797b9bf469904cfd5"),
    ),
    "aarch64-unknown-linux-gnu": (
        ("libzvec_c_api.so", "621af6ba8249ce44dc17fb05da6c51c723cc466843e7f46ee44a40bd7eee1169"),
    ),
    "aarch64-apple-darwin": (
        ("libzvec_c_api.dylib", "c9e4bf9387ef7261a284de407ec7e48ac9a48309d8daaa4c5ed85a8fa5bb4763"),
    ),
    "x86_64-pc-windows-msvc": (
        ("zvec_c_api.dll", "3745106b3beee6be2d50ca678b46d3f0289afb5136ff51e1d1ec037a27b29e4a"),
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
    # zvec-rust's v0.7.0 submodule points at this exact zvec core commit.
    ("retrieval/licenses/zvec-NOTICE", "https://raw.githubusercontent.com/alibaba/zvec/8321c1314a559fd5f909e92498f43e5194bf9b99/NOTICE", "332b1a498b446fab1232b671c2ba74102fc563c198dc6f53980d1282075958ad"),
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
