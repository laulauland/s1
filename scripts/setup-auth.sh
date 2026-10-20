#!/usr/bin/env bash
# Prompt for provider keys and write them to ~/.s1/auth.json (pi auth.json
# shape, mode 0600). Stands in for `s1 auth set` until stage 1 ships it.
#
#   scripts/setup-auth.sh           prompt for each value
#   scripts/setup-auth.sh --check   verify stored keys against the providers
set -euo pipefail
umask 077

DIR="${S1_HOME:-$HOME/.s1}"
FILE="$DIR/auth.json"

# provider|field|secret?|description
FIELDS=(
  "typesafe|key|1|TypeSafe Jev API key (stages 1-2)"
  "workers-ai|key|1|Cloudflare API token with Workers AI permission (stages 2, 5)"
  "workers-ai|accountId|0|Cloudflare account ID (stages 2, 5)"
  "openai|key|1|OpenAI API key with Decisions beta access (stage 4)"
)

mkdir -p "$DIR"
chmod 700 "$DIR"
[[ -f "$FILE" ]] || echo '{}' >"$FILE"
chmod 600 "$FILE"
AUTH=$(cat "$FILE")

get() { jq -r --arg p "$1" --arg f "$2" '.[$p][$f] // empty' <<<"$AUTH"; }

mask() {
  local v="$1"
  if [[ -z "$v" ]]; then echo "not set"
  elif (( ${#v} <= 8 )); then echo "set"
  else echo "${v:0:4}…${v: -4}"
  fi
}

# Send the key through stdin so it never shows in the process list.
http_code() {
  printf 'Authorization: Bearer %s\n' "$1" |
    curl -s -o /dev/null -w '%{http_code}' -H @- "$2"
}

check() {
  local v
  v=$(get openai key)
  [[ -n "$v" ]] && echo "openai: HTTP $(http_code "$v" https://api.openai.com/v1/models)"
  v=$(get workers-ai key)
  [[ -n "$v" ]] && echo "workers-ai: HTTP $(http_code "$v" https://api.cloudflare.com/client/v4/user/tokens/verify)"
  v=$(get typesafe key)
  [[ -n "$v" ]] && echo "typesafe: stored (no check endpoint known; stage 1 smoke test covers it)"
  return 0
}

if [[ "${1:-}" == "--check" ]]; then
  check
  exit 0
fi

echo "Writing to $FILE"
echo "Enter keeps the current value. '-' clears it."
echo
for entry in "${FIELDS[@]}"; do
  IFS='|' read -r provider field secret desc <<<"$entry"
  echo "$provider.$field — $desc [$(mask "$(get "$provider" "$field")")]"
  if [[ "$secret" == 1 ]]; then
    read -rsp "> " input; echo
  else
    read -rp "> " input
  fi
  if [[ "$input" == "-" ]]; then
    AUTH=$(P="$provider" F="$field" jq 'del(.[env.P][env.F])
      | if .[env.P] == {type: "api_key"} then del(.[env.P]) else . end' <<<"$AUTH")
  elif [[ -n "$input" ]]; then
    AUTH=$(P="$provider" F="$field" V="$input" jq '.[env.P] = ((.[env.P] // {}) + {type: "api_key", (env.F): env.V})' <<<"$AUTH")
  fi
done

tmp=$(mktemp "$FILE.XXXXXX")
printf '%s\n' "$AUTH" >"$tmp"
chmod 600 "$tmp"
mv "$tmp" "$FILE"

echo
echo "Saved. Checking keys:"
check
