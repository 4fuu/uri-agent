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
tmp_binary=
cleanup() {
    rm -rf "$tmp_dir"
    if [ -n "$tmp_binary" ]; then
        rm -f "$tmp_binary"
    fi
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

mkdir -p "$INSTALL_DIR"
tmp_binary=$(mktemp "$INSTALL_DIR/.uri-agent.tmp.XXXXXX")
cp "$tmp_dir/archive/uri-agent" "$tmp_binary"
chmod 0755 "$tmp_binary"
mv -f "$tmp_binary" "$INSTALL_DIR/uri-agent"
tmp_binary=

printf 'Installed uri-agent %s to %s/uri-agent\n' "$version" "$INSTALL_DIR"
case :${PATH:-}: in
    *:"$INSTALL_DIR":*) ;;
    *) printf 'Add %s to PATH to run uri-agent.\n' "$INSTALL_DIR" ;;
esac
