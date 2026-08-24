#!/bin/bash
set -euo pipefail

if [[ ${1:-} == --version ]]; then
    printf '%s\n' 'tyrion-effect-adapter 1.0.0'
    exit 0
fi

[[ ${1:-} == --execute-stdin ]]
IFS= read -r credential
IFS= read -r destination
IFS= read -r content_type
body=$(cat)
sleep 300 </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!" >"${TYRION_EFFECT_ROOT:?}/descendant.pid"
{
    printf 'header = "Authorization: Bearer %s"\n' "$credential"
} | /usr/bin/curl \
    --disable \
    --config - \
    --silent \
    --show-error \
    --max-time 5 \
    --max-redirs 0 \
    --proto '=http,https' \
    --request POST \
    --header "Content-Type: $content_type" \
    --data-binary "$body" \
    --write-out $'\nTYRION_HTTP_STATUS:%{http_code}' \
    "$destination"
