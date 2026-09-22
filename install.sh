#!/bin/sh
# Install sluice: download the latest release for this machine, check it
# against the published checksums, and start the setup flow.
#
#   curl -fsSL https://raw.githubusercontent.com/max-bromberg/sluice/main/install.sh | sh
#
# Nothing is installed or changed by this script itself. `sluice setup` shows
# its whole plan and asks before doing anything, and needs your password once.
#
# Environment:
#   SLUICE_VERSION   a release tag such as v0.2.0 (default: the latest)
#   SLUICE_REPO      owner/name on GitHub (default: max-bromberg/sluice)
#   SLUICE_BASE_URL  download from here instead of GitHub (mirrors, testing)
#
# Arguments are passed to `sluice setup` (for example `--yes`).
set -eu

repo="${SLUICE_REPO:-max-bromberg/sluice}"
version="${SLUICE_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() { printf 'sluice install: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "sluice runs on Linux only"

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "no release for $(uname -m) yet" ;;
esac

if [ -n "${SLUICE_BASE_URL:-}" ]; then
    base="$SLUICE_BASE_URL"   # a mirror, or a local release when testing
elif [ "$version" = "latest" ]; then
    base="https://github.com/$repo/releases/latest/download"
else
    base="https://github.com/$repo/releases/download/$version"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "needs curl or wget"
fi
command -v sha256sum >/dev/null 2>&1 || die "needs sha256sum"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

asset="sluice-$arch-linux"
say "Downloading sluice ($version, $arch)…"
fetch "$base/$asset" "$tmp/sluice" || die "could not download $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download the checksums"

expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
actual="$(sha256sum "$tmp/sluice" | cut -d' ' -f1)"
[ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset; not running it"
chmod +x "$tmp/sluice"
say "Verified. Starting setup."

# Setup asks questions, so it needs the terminal even when this script
# arrived through a pipe.
if [ -t 0 ]; then
    "$tmp/sluice" setup "$@"
elif (: </dev/tty) 2>/dev/null; then
    "$tmp/sluice" setup "$@" </dev/tty
else
    # No terminal at all: setup explains, or proceeds if given --yes.
    "$tmp/sluice" setup "$@"
fi
