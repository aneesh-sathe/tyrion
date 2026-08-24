#!/bin/bash
set -euo pipefail

state=$(dirname "${XDG_CONFIG_HOME:?}")/fake-effect-openshell
log="$state/commands.log"
mkdir -p "$state"
printf '%q ' "$@" >>"$log"
printf '\n' >>"$log"

if [[ ${1:-} == --version ]]; then
    printf '%s\n' 'openshell 0.0.104'
    exit 0
fi

if [[ ${1:-} == logs ]]; then
    printf '%s\n' 'Landlock ruleset built'
    printf '%s\n' 'network policy enforced'
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
        mkdir -p "$state/sandboxes/$name"
        ;;
    upload)
        name=$1
        source_path=$2
        remote_path=$3
        destination="$state/sandboxes/$name/${remote_path#/sandbox/}"
        mkdir -p "$(dirname "$destination")"
        cp "$source_path" "$destination"
        ;;
    exec)
        name=
        while (($#)); do
            case "$1" in
                -n|--name)
                    name=$2
                    shift 2
                    ;;
                --workdir)
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
        mapped=()
        for argument in "$@"; do
            mapped+=("${argument//\/sandbox/$root}")
        done
        if [[ " $* " == *tyrion-effect-containment-probe* ]]; then
            printf '%s\n' 'effect-containment-ok'
            exit 0
        fi
        exec env -i \
            PATH=/usr/local/bin:/usr/bin:/bin \
            TYRION_EFFECT_ROOT="$root" \
            "${mapped[@]}"
        ;;
    delete)
        name=$1
        root="$state/sandboxes/$name"
        if [[ -f $root/descendant.pid ]]; then
            kill "$(cat "$root/descendant.pid")" 2>/dev/null || true
            printf '%s\n' 'descendant-terminated' >>"$log"
        fi
        rm -rf "$root"
        ;;
    *)
        printf 'unsupported fake effect operation: %s\n' "$operation" >&2
        exit 2
        ;;
esac
