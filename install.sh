#!/bin/sh
set -eu

REPOSITORY="yibudak/ortak"

fail() {
    printf 'ortak installer: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

need curl
need tar
need awk
need install
need mv

case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-gnu" ;;
    *) fail "unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

if [ -n "${ORTAK_INSTALL_DIR:-}" ]; then
    install_dir="$ORTAK_INSTALL_DIR"
else
    [ -n "${HOME:-}" ] || fail "HOME is not set; set ORTAK_INSTALL_DIR"
    install_dir="$HOME/.local/bin"
fi

version="${ORTAK_VERSION:-}"
if [ -n "$version" ]; then
    case "$version" in
        v*) ;;
        *) version="v$version" ;;
    esac
    case "$version" in
        *[!A-Za-z0-9._-]*) fail "invalid ORTAK_VERSION: $version" ;;
    esac
    download_base="https://github.com/$REPOSITORY/releases/download/$version"
else
    download_base="https://github.com/$REPOSITORY/releases/latest/download"
fi

target="$arch-$os"
archive="ortak-$target.tar.gz"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/ortak-install.XXXXXX")" || fail "could not create temporary directory"
staged="$install_dir/.ortak.new.$$"
cleanup() {
    rm -rf "$tmp_dir"
    rm -f "$staged"
}
trap cleanup 0 HUP INT TERM

printf 'Downloading ortak for %s...\n' "$target"
curl --proto '=https' --tlsv1.2 -fsSL "$download_base/$archive" -o "$tmp_dir/$archive"
curl --proto '=https' --tlsv1.2 -fsSL "$download_base/SHA256SUMS" -o "$tmp_dir/SHA256SUMS"

expected="$(awk -v name="$archive" '$2 == name { print $1; exit }' "$tmp_dir/SHA256SUMS")"
[ -n "$expected" ] || fail "checksum not found for $archive"

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp_dir/$archive" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp_dir/$archive" | awk '{ print $1 }')"
else
    fail "sha256sum or shasum is required"
fi

[ "$actual" = "$expected" ] || fail "checksum verification failed for $archive"

tar -xzf "$tmp_dir/$archive" -C "$tmp_dir"
[ -f "$tmp_dir/ortak" ] || fail "release archive does not contain the ortak binary"

mkdir -p "$install_dir"
install -m 0755 "$tmp_dir/ortak" "$staged"
mv -f "$staged" "$install_dir/ortak"

printf 'Installed ortak to %s/ortak\n' "$install_dir"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        printf '\nAdd ortak to your PATH:\n'
        printf '  export PATH="%s:$PATH"\n' "$install_dir"
        ;;
esac
