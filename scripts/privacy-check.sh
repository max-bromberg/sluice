#!/usr/bin/env bash
# Keep personal and machine-specific data out of the repository.
#
#   scripts/privacy-check.sh            check every file git would publish
#   scripts/privacy-check.sh --commits  also check commit identities and
#                                       messages on the current branch
#
# Generic patterns catch what should never be published by anyone: email
# addresses, home directories, IP and MAC addresses, webhook tokens, keys.
# Terms specific to one person or machine cannot be listed here without
# publishing them, so they live in a git-ignored denylist instead:
# `local/privacy-denylist` (or $SLUICE_PRIVACY_DENYLIST), one extended regex
# per line, `#` for comments.
#
# A line that legitimately matches can carry `privacy-ok` to be exempted.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0
report() { printf '%s\n' "$1" >&2; fail=1; }

# Files that would be published: tracked, plus untracked-but-not-ignored.
mapfile -t files < <(git ls-files -co --exclude-standard | grep -v -E '^(Cargo\.lock|scripts/privacy-check\.sh)$')

generic=(
  'email address|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
  'home directory|/(home|Users)/[a-z_][a-z0-9_.-]*'
  'IPv4 address|\b([0-9]{1,3}\.){3}[0-9]{1,3}\b'
  'MAC address|\b([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}\b'
  'webhook token|(discord(app)?\.com/api/webhooks/[0-9]{6,}|hooks\.slack\.com/services/T[A-Z0-9]+)'
  'private key|-----BEGIN [A-Z ]*PRIVATE KEY-----'
  'access token|\b(gh[pousr]_[A-Za-z0-9]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16})'
)
# Addresses and hosts that are safe by construction.
allow='(noreply@anthropic\.com|@users\.noreply\.github\.com|@example\.(com|org|net)|\b127\.0\.0\.1\b|\b0\.0\.0\.0\b|privacy-ok)'

for rule in "${generic[@]}"; do
  name=${rule%%|*}
  pattern=${rule#*|}
  while IFS= read -r hit; do
    [[ -z $hit ]] && continue
    if ! grep -qE "$allow" <<<"$hit"; then
      report "privacy: $name: $hit"
    fi
  done < <(grep -nHE "$pattern" -- "${files[@]}" 2>/dev/null || true)
done

denylist=${SLUICE_PRIVACY_DENYLIST:-local/privacy-denylist}
terms=()
if [[ -f $denylist ]]; then
  while IFS= read -r line; do
    [[ -z $line || $line == \#* ]] && continue
    terms+=("$line")
  done <"$denylist"
fi
for term in "${terms[@]}"; do
  while IFS= read -r hit; do
    [[ -n $hit ]] && report "privacy: denylisted term: ${hit%%:*}:$(cut -d: -f2 <<<"$hit") (content withheld)"
  done < <(grep -nHiE -- "$term" "${files[@]}" 2>/dev/null || true)
done

if [[ ${1:-} == --commits ]]; then
  range=${2:-HEAD}
  while IFS='|' read -r sha author committer; do
    for who in "$author" "$committer"; do
      if ! grep -qE '(@users\.noreply\.github\.com|^noreply@github\.com)$' <<<"$who"; then
        report "privacy: commit ${sha:0:8} has a non-noreply identity: $who"
      fi
    done
  done < <(git log --format='%H|%ae|%ce' "$range" 2>/dev/null || true)
  for term in "${terms[@]}"; do
    if git log --format='%B' "$range" 2>/dev/null | grep -qiE -- "$term"; then
      report "privacy: a commit message on this branch contains a denylisted term"
    fi
  done
fi

if ((fail)); then
  echo "privacy check failed; nothing personal or machine-specific may be published" >&2
  exit 1
fi
echo "privacy check passed (${#files[@]} files, ${#terms[@]} local terms)"
