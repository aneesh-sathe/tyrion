#!/bin/bash
# Deterministic stand-in for the Docker CLI on the one-shot Effect Sandbox
# path: create, transfer, execute, inspect, remove, plus the per-operation
# network and destination-pinned relay.
set -euo pipefail

state="$(cd "$(dirname "$0")" && pwd)/fake-effect-docker"
log="$state/commands.log"

if [[ ${1:-} == --version ]]; then
    printf '%s\n' 'Docker version 28.0.4, build fixture'
    exit 0
fi

mkdir -p "$state/containers" "$state/networks"
printf '%q ' "$@" >>"$log"
printf '\n' >>"$log"

image_id="sha256:$(printf '2%.0s' $(seq 64))"
operation=${1:-}
shift || true

case "$operation" in
    image)
        [[ ${1:-} == inspect ]]
        printf '%s\n' "$image_id" ;;
    run)
        name=; seen=(); add_host=
        while (($#)); do
            case "$1" in
                --name) name=$2; shift 2 ;;
                --add-host) add_host=$2; shift 2 ;;
                --detach) shift ;;
                --network|--user|--tmpfs|--workdir|--env|--pids-limit|--memory|--memory-swap|--cpus|--cpuset-cpus|--cap-drop|--security-opt)
                    seen+=("$1"); shift 2 ;;
                --read-only) seen+=("$1"); shift ;;
                *) break ;;
            esac
        done
        [[ -n $name ]]
        if [[ $name != *-relay ]]; then
            for required in --read-only --cap-drop --security-opt --user --tmpfs --pids-limit --memory --cpus --network; do
                if [[ " ${seen[*]-} " != *" $required "* ]]; then
                    printf 'fake docker: effect sandbox missing %s\n' "$required" >&2; exit 64
                fi
            done
            [[ -n $add_host ]] || { printf 'fake docker: no pinned destination\n' >&2; exit 64; }
        fi
        mkdir -p "$state/containers/$name"
        printf '%s\n' "$name" ;;
    inspect)
        format=
        while (($#)); do
            case "$1" in
                --format) format=$2; shift 2 ;;
                --type) shift 2 ;;
                *) break ;;
            esac
        done
        name=$1
        [[ -d $state/containers/$name ]] || { printf 'Error: No such container: %s\n' "$name" >&2; exit 1; }
        case "$format" in
            '{{.Image}}') printf '%s\n' "$image_id" ;;
            *) printf '\n' ;;
        esac ;;
    logs)
        while [[ ${1:-} == --* ]]; do shift; [[ ${1:-} == -* || -z ${1:-} ]] || shift; done
        printf '%s\n' 'effect sandbox log' ;;
    exec)
        while (($#)); do
            case "$1" in
                --interactive|-i) shift ;;
                --workdir|--env) shift 2 ;;
                *) break ;;
            esac
        done
        name=$1; shift
        root="$state/containers/$name"
        mkdir -p "$root"
        if [[ " $* " == *tyrion-effect-containment-probe* ]]; then
            printf '%s\n' 'effect-containment-ok'; exit 0
        fi
        if [[ ${1:-} == sh && ${2:-} == -c && ${3:-} == *"cat > "* ]]; then
            target=$(printf '%s' "$3" | sed -E "s/.*cat > '([^']*)'.*/\1/")
            dest="$root/${target#/sandbox/}"
            mkdir -p "$(dirname "$dest")"
            cat > "$dest"; chmod 700 "$dest"; exit 0
        fi
        mapped=()
        for argument in "$@"; do mapped+=("${argument///sandbox/$root}"); done
        exec env -i PATH=/usr/local/bin:/usr/bin:/bin TYRION_EFFECT_ROOT="$root" "${mapped[@]}" ;;
    rm)
        while [[ ${1:-} == --* ]]; do shift; done
        name=$1; root="$state/containers/$name"
        [[ -d $root ]] || { printf 'Error: No such container: %s\n' "$name" >&2; exit 1; }
        if [[ -f $root/descendant.pid ]]; then
            kill "$(cat "$root/descendant.pid")" 2>/dev/null || true
            printf '%s\n' 'descendant-terminated' >>"$log"
        fi
        rm -rf "${state:?}/containers/$name"
        printf '%s\n' "$name" ;;
    network)
        action=$1; shift
        case "$action" in
            create) name=${*: -1}; mkdir -p "$state/networks/$name"; printf '%s\n' "$name" ;;
            connect) printf '\n' ;;
            rm)
                for name in "$@"; do
                    [[ -n $name ]] || continue
                    [[ -d $state/networks/$name ]] || { printf 'Error: No such network: %s\n' "$name" >&2; exit 1; }
                    rm -rf "${state:?}/networks/$name"
                done ;;
            *) printf 'unsupported fake network action: %s\n' "$action" >&2; exit 2 ;;
        esac ;;
    *)
        printf 'unsupported fake effect operation: %s\n' "$operation" >&2; exit 2 ;;
esac
