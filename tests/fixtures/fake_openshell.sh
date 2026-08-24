#!/bin/bash
set -euo pipefail

state=$(dirname "${XDG_CONFIG_HOME:?}")/fake-openshell
log="$state/commands.log"
printf '%q ' "$@" >>"$log"
printf '\n' >>"$log"

if [[ ${1:-} == --version ]]; then
    printf '%s\n' 'openshell 0.0.104'
    exit 0
fi

if [[ ${1:-} == logs ]]; then
    printf '%s\n' 'Landlock ruleset built'
    exit 0
fi

[[ ${1:-} == sandbox ]]
operation=${2:-}
shift 2

case "$operation" in
    create)
        name=
        while (($#)); do
            if [[ $1 == --name ]]; then
                name=$2
                shift 2
            else
                shift
            fi
        done
        [[ -n $name ]]
        if ((${#name} > 19)); then
            printf 'sandbox name exceeds maximum length: %s > 19\n' "${#name}" >&2
            exit 64
        fi
        mkdir -p "$state/sandboxes/$name"
        ;;
    upload)
        name=$1
        local_path=$2
        remote_path=$3
        destination="$state/sandboxes/$name/${remote_path#/sandbox/}"
        mkdir -p "$(dirname "$destination")"
        cp "$local_path" "$destination"
        ;;
    download)
        name=$1
        remote_path=$2
        local_path=$3
        if [[ -e $state/corrupt-candidate && $remote_path == /sandbox/candidate.bundle ]]; then
            printf '%s\n' 'not a Git bundle' >"$local_path"
        else
            cp "$state/sandboxes/$name/${remote_path#/sandbox/}" "$local_path"
        fi
        ;;
    exec)
        name=
        workdir=
        while (($#)); do
            case "$1" in
                -n|--name)
                    name=$2
                    shift 2
                    ;;
                --workdir)
                    workdir=$2
                    shift 2
                    ;;
                --no-tty)
                    shift
                    ;;
                --)
                    shift
                    break
                    ;;
                *)
                    shift
                    ;;
            esac
        done
        root="$state/sandboxes/$name"
        if [[ " $* " == *tyrion-containment-probe* ]]; then
            required_probe_terms=(
                '/sys/fs/cgroup/pids.max'
                'getconf _NPROCESSORS_ONLN'
                '/proc/meminfo'
                'df -Pk /sandbox'
                '/var/run/docker.sock'
                '/run/containerd/containerd.sock'
                '/home/sandbox/.ssh'
                '/home/sandbox/.aws'
                '/home/sandbox/.config/gh'
                '/home/sandbox/.codex'
                '/home/sandbox/.claude'
                'OPENAI_API_KEY'
                'ANTHROPIC_API_KEY'
                'AWS_ACCESS_KEY_ID'
                'GH_TOKEN'
                'GITHUB_TOKEN'
                'SSH_AUTH_SOCK'
                '/opt/openshell/auth/sandbox.jwt'
                '/opt/openshell/tls/tls.key'
                '/etc/tyrion-probe'
                '/sandbox/tyrion-probe'
                'command -v curl'
                'https://example.com'
                'descendant-live'
            )
            for required in "${required_probe_terms[@]}"; do
                if [[ $* != *"$required"* ]]; then
                    printf 'missing containment probe: %s\n' "$required" >&2
                    exit 89
                fi
            done
            if [[ -e $state/fail-preflight ]]; then
                printf '%s\n' 'simulated containment failure' >&2
                exit 90
            fi
            sleep 300 </dev/null >/dev/null 2>&1 &
            printf '%s\n' "$!" >"$root/preflight-descendant.pid"
            printf '%s\n' 'containment-ok'
            exit 0
        fi
        mapped=()
        for argument in "$@"; do
            argument=${argument//\/sandbox/$root}
            mapped+=("$argument")
        done
        if [[ -n $workdir ]]; then
            workdir=${workdir//\/sandbox/$root}
            cd "$workdir"
        fi
        if [[ -e $state/fail-integrated-verification && $name == tyrion-i-* && -n $workdir ]]; then
            printf '%s\n' 'simulated integrated verification failure' >&2
            exit 7
        fi
        if [[ -e $state/hold-candidate-verification && $name == tyrion-c-* && -n $workdir ]]; then
            printf '%s\n' "$$" >"$root/verification-descendant.pid"
            sleep 300
            exit 91
        fi
        exec env \
            TYRION_WORKSPACE_ROOT="$root" \
            TYRION_FAKE_STATE="$state" \
            CODEX_AUTH_ACCESS_TOKEN=openshell:resolve:env:CODEX_AUTH_ACCESS_TOKEN \
            CODEX_AUTH_REFRESH_TOKEN=openshell:resolve:env:CODEX_AUTH_REFRESH_TOKEN \
            CODEX_AUTH_ACCOUNT_ID=openshell:resolve:env:CODEX_AUTH_ACCOUNT_ID \
            CODEX_AUTH_ID_TOKEN=openshell:resolve:env:CODEX_AUTH_ID_TOKEN \
            "${mapped[@]}"
        ;;
    delete)
        name=${1:-}
        if [[ $name == tyrion-c-* && -f $state/sandboxes/$name/verification-descendant.pid ]]; then
            rm -f "$state/hold-candidate-verification"
        fi
        for descendant in "$state/sandboxes/$name"/*descendant.pid; do
            [[ -f $descendant ]] || continue
            kill "$(cat "$descendant")" 2>/dev/null || true
            printf '%s\n' 'descendant-terminated' >>"$log"
        done
        rm -rf "$state/sandboxes/$name"
        ;;
    *)
        printf 'unsupported fake operation: %s\n' "$operation" >&2
        exit 2
        ;;
esac
