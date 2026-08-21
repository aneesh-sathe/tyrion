#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
    if [ -z "${TYRION_WORKSPACE_ROOT:-}" ]; then
        printf '%s\n' 'guest Codex was executed outside the sandbox' >&2
        exit 86
    fi
    fake_state=$(dirname "$(dirname "$TYRION_WORKSPACE_ROOT")")
    if [ -e "$fake_state/wrong-codex-version" ]; then
        printf '%s\n' 'codex-cli 0.146.0'
        exit 0
    fi
    printf '%s\n' 'codex-cli 0.147.0'
    exit 0
fi

repo=
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -C|--cd)
            repo=$2
            shift 2
            ;;
        -o|--output-last-message)
            output=$2
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

test -n "$repo"
test -n "$output"
if env | grep -E '^(OPENAI_API_KEY|AWS_ACCESS_KEY_ID|GH_TOKEN|GITHUB_TOKEN|SSH_AUTH_SOCK)=' >/dev/null; then
    printf '%s\n' 'ambient credential reached Codex' >&2
    exit 70
fi
prompt=$(cat)
if printf '%s\n' "$prompt" | grep -q 'Reconcile competing Results'; then
    contender_count=0
    for contender in $(printf '%s\n' "$prompt" | grep -o '/sandbox/contenders/[0-9][0-9]*\.bundle'); do
        local_contender="$(dirname "$repo")/${contender#/sandbox/}"
        test -f "$local_contender"
        git -C "$repo" bundle verify "$local_contender" >/dev/null
        contender_count=$((contender_count + 1))
    done
    test "$contender_count" -ge 2
fi
sleep 300 </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!" >"$(dirname "$repo")/descendant.pid"
fake_state=$(dirname "$(dirname "$(dirname "$repo")")")
if [ -e "$fake_state/slow-codex" ]; then
    sleep 5
fi
fixture_path=$(printf '%s\n' "$prompt" | sed -n 's/.*TYRION_FIXTURE_WRITE=\([^ ]*\).*/\1/p' | head -n 1)
fixture_content=$(printf '%s\n' "$prompt" | sed -n 's/.*TYRION_FIXTURE_CONTENT=\([^ ]*\).*/\1/p' | head -n 1)
fixture_delay=$(printf '%s\n' "$prompt" | sed -n 's/.*TYRION_FIXTURE_DELAY=\([^ ]*\).*/\1/p' | head -n 1)
fixture_delete=$(printf '%s\n' "$prompt" | sed -n 's/.*TYRION_FIXTURE_DELETE=\([^ ]*\).*/\1/p' | head -n 1)
fixture_read_only=$(printf '%s\n' "$prompt" | sed -n 's/.*TYRION_FIXTURE_READ_ONLY=\([^ ]*\).*/\1/p' | head -n 1)
reconciliation_path=$(printf '%s\n' "$prompt" | sed -n 's/.*Authorized reconciliation write scope: \([^ ]*\).*/\1/p' | head -n 1)
if [ -z "$fixture_path" ] && [ -n "$reconciliation_path" ]; then
    fixture_path=$reconciliation_path
    fixture_content=reconciled
fi
if [ -n "$fixture_delay" ]; then
    sleep "$fixture_delay"
fi
if [ -n "$fixture_read_only" ]; then
    :
elif [ -n "$fixture_path" ]; then
    mkdir -p "$(dirname "$repo/$fixture_path")"
    printf '%s\n' "$fixture_content" >"$repo/$fixture_path"
else
    printf '%s\n' 'contained codex result' >"$repo/issue-4.txt"
fi
if [ -n "$fixture_delete" ]; then
    rm "$repo/$fixture_delete"
fi
if [ -e "$fake_state/unauthorized-change" ]; then
    printf '%s\n' 'outside authority' >"$repo/outside.txt"
fi
if [ -e "$fake_state/reverted-unauthorized-change" ]; then
    git -C "$repo" config user.name 'Adversarial Fixture'
    git -C "$repo" config user.email 'fixture@tyrion.invalid'
    printf '%s\n' 'transient outside authority' >"$repo/outside.txt"
    git -C "$repo" add outside.txt
    git -C "$repo" commit -qm 'test: touch unauthorized path'
    rm "$repo/outside.txt"
    git -C "$repo" add outside.txt
    git -C "$repo" commit -qm 'test: revert unauthorized path'
fi
printf '%s\n' "{\"summary\":\"completed ${fixture_path:-inspection}\",\"known_effects\":[]}" >"$output"
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}'
