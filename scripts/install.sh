#!/bin/sh
set -eu

REPOSITORY=${URI_AGENT_REPOSITORY:-4fuu/uri-agent}
INSTALL_DIR=${URI_AGENT_INSTALL_DIR:-"$HOME/.local/bin"}
REQUESTED_VERSION=${1:-latest}

fail() {
    printf 'uri-agent installer: %s\n' "$*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

[ "$(uname -s)" = Linux ] || fail "this installer supports Linux; use Homebrew on macOS"
case $(uname -m) in
    x86_64 | amd64) target=x86_64-unknown-linux-gnu ;;
    aarch64 | arm64) target=aarch64-unknown-linux-gnu ;;
    *) fail "unsupported Linux architecture: $(uname -m)" ;;
esac

if [ "$REQUESTED_VERSION" = latest ]; then
    release_url=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null \
        -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest")
    tag=${release_url##*/}
else
    tag=$REQUESTED_VERSION
    case $tag in v*) ;; *) tag=v$tag ;; esac
fi

version=${tag#v}
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || fail "invalid release version: $tag"

asset="uri-agent-$version-$target.tar.gz"
download_url="https://github.com/$REPOSITORY/releases/download/$tag"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/uri-agent-install.XXXXXX")
stage_dir=
old_dir=
link_tmp=
cleanup() {
    rm -rf "$tmp_dir"
    [ -z "$stage_dir" ] || rm -rf "$stage_dir"
    [ -z "$old_dir" ] || rm -rf "$old_dir"
    [ -z "$link_tmp" ] || rm -f "$link_tmp"
}
trap cleanup EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -fL "$download_url/$asset" -o "$tmp_dir/$asset"
curl --proto '=https' --tlsv1.2 -fL "$download_url/SHA256SUMS" \
    -o "$tmp_dir/SHA256SUMS"

expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1 }' \
    "$tmp_dir/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $asset"
[ "$(printf '%s\n' "$expected" | wc -l | tr -d ' ')" = 1 ] \
    || fail "SHA256SUMS has multiple entries for $asset"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp_dir/$asset" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp_dir/$asset" | awk '{ print $1 }')
else
    fail "sha256sum or shasum is required to verify the download"
fi
[ "$actual" = "$expected" ] || fail "checksum verification failed for $asset"

mkdir "$tmp_dir/archive"
tar -xzf "$tmp_dir/$asset" -C "$tmp_dir/archive"
[ -f "$tmp_dir/archive/uri-agent" ] || fail "release archive does not contain uri-agent"
[ -f "$tmp_dir/archive/libzvec_c_api.so" ] || fail "release archive does not contain libzvec_c_api.so"
[ -f "$tmp_dir/archive/retrieval/models/potion-code-16M-v2/model.safetensors" ] \
    || fail "release archive does not contain the embedding model"
[ -f "$tmp_dir/archive/retrieval/jieba/jieba.dict.utf8" ] \
    || fail "release archive does not contain the Jieba dictionary"

mkdir -p "$INSTALL_DIR"
version_dir="$INSTALL_DIR/uri-agent-$version"
stage_dir=$(mktemp -d "$INSTALL_DIR/.uri-agent-$version.tmp.XXXXXX")
cp -R "$tmp_dir/archive/." "$stage_dir/"
chmod 0755 "$stage_dir/uri-agent"
if [ -e "$version_dir" ] || [ -L "$version_dir" ]; then
    old_dir="$INSTALL_DIR/.uri-agent-$version.old.$$"
    mv "$version_dir" "$old_dir"
fi
if ! mv "$stage_dir" "$version_dir"; then
    [ -z "$old_dir" ] || mv "$old_dir" "$version_dir"
    fail "could not activate $version_dir"
fi
stage_dir=
link_tmp="$INSTALL_DIR/.uri-agent-link.$$"
ln -s "uri-agent-$version/uri-agent" "$link_tmp"
mv -f "$link_tmp" "$INSTALL_DIR/uri-agent"
link_tmp=
if [ -n "$old_dir" ]; then
    rm -rf "$old_dir"
    old_dir=
fi

printf 'Installed uri-agent %s to %s (launcher: %s/uri-agent)\n' "$version" "$version_dir" "$INSTALL_DIR"
case :${PATH:-}: in
    *:"$INSTALL_DIR":*) ;;
    *) printf 'Add %s to PATH to run uri-agent.\n' "$INSTALL_DIR" ;;
esac
