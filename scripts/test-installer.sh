#!/usr/bin/env bash
# End-to-end test of install.sh against a local, throwaway "release".
#
#   scripts/test-installer.sh path/to/sluice
#
# Checks that a good download is verified and handed to `sluice setup`, and
# that a download whose checksum does not match is refused and never run.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

binary=${1:?usage: scripts/test-installer.sh path/to/sluice}
arch=$(uname -m)
[[ $arch == amd64 ]] && arch=x86_64
[[ $arch == arm64 ]] && arch=aarch64

release=$(mktemp -d)
cleanup() {
    [[ -n ${server:-} ]] && kill "$server" 2>/dev/null
    rm -rf "$release"
}
trap cleanup EXIT

cp "$binary" "$release/sluice-$arch-linux"
(cd "$release" && sha256sum "sluice-$arch-linux" > SHA256SUMS)

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$release" >/dev/null 2>&1 &
server=$!
for _ in $(seq 50); do
    curl -fs "http://127.0.0.1:$port/SHA256SUMS" >/dev/null && break
    sleep 0.1
done

run() { SLUICE_BASE_URL="http://127.0.0.1:$port" setsid sh install.sh </dev/null 2>&1 || true; }

out=$(run)
grep -q "Verified. Starting setup." <<<"$out" || { echo "FAIL: a good download was not verified"; echo "$out"; exit 1; }
grep -q "setup asks a few questions" <<<"$out" || { echo "FAIL: setup did not start"; echo "$out"; exit 1; }
echo "ok: a verified download starts setup"

echo "0000000000000000000000000000000000000000000000000000000000000000  sluice-$arch-linux" > "$release/SHA256SUMS"
out=$(run)
grep -q "checksum mismatch" <<<"$out" || { echo "FAIL: a bad checksum was not refused"; echo "$out"; exit 1; }
if grep -q "Starting setup" <<<"$out"; then echo "FAIL: a bad download was run"; exit 1; fi
echo "ok: a download with the wrong checksum is refused"
