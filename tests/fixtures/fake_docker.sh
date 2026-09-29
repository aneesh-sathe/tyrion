#!/bin/bash
# Deterministic stand-in for the Docker CLI. It models the containment seam
# Tyrion depends on -- create, transfer, execute, inspect, remove -- so the
# default suite exercises the real code path without a daemon.
set -euo pipefail

state="$(cd "$(dirname "$0")" && pwd)/fake-docker"
log="$state/commands.log"

if [[ ${1:-} == --version ]]; then
    printf '%s\n' 'Docker version 28.0.4, build fixture'
    exit 0
fi

mkdir -p "$state/containers" "$state/networks"
printf '%q ' "$@" >>"$log"
printf '\n' >>"$log"

image_id="sha256:$(printf '1%.0s' $(seq 64))"

operation=${1:-}
shift || true

case "$operation" in
    info)
        # info --format '{{.NCPU}} {{.MemTotal}}': a host roomy enough that no
        # existing test is held. Tests exercise holds with --host-cpus and
        # --host-memory-mib instead.
        printf '%s\n' '16 34359738368'
        ;;
    image)
        # image inspect --format {{.Id}} <reference>
        [[ ${1:-} == inspect ]]
        printf '%s\n' "$image_id"
        ;;
    create|run)
        name=
        network=
        label=
        detach=0
        aliases=()
        seen=()
        while (($#)); do
            case "$1" in
                --name) name=$2; shift 2 ;;
                --label) seen+=("--label"); label=$2; shift 2 ;;
                --network) network=$2; shift 2 ;;
                --add-host) aliases+=("$2"); shift 2 ;;
                --detach) detach=1; shift ;;
                --cpuset-cpus)
                    # Real Docker refuses a CPU the engine does not have.
                    for cpu in ${2//,/ }; do
                        if ((cpu >= 16)); then
                            printf 'docker: Error response from daemon: Requested CPUs are not available - requested %s, available: 0-15\n' "$2" >&2
                            exit 125
                        fi
                    done
                    seen+=("$1")
                    shift 2
                    ;;
                --read-only|--cap-drop|--security-opt|--user|--tmpfs|--workdir|--env|--pids-limit|--memory|--memory-swap|--cpus)
                    seen+=("$1")
                    [[ $1 == --read-only ]] || shift
                    shift
                    ;;
                *) break ;;
            esac
        done
        [[ -n $name ]]
        # Every sandbox must be created with the whole hardened profile. An
        # egress relay holds no files and only forwards bytes, so it runs
        # without a writable mount or a CPU share, but is hardened otherwise.
        required_flags=(--read-only --cap-drop --security-opt --user --tmpfs --pids-limit --memory --cpus)
        if [[ $name =~ -net-r[0-9]+$ ]]; then
            required_flags=(--read-only --cap-drop --security-opt --user --pids-limit --memory)
        fi
        for required in "${required_flags[@]}"; do
            if [[ " ${seen[*]-} " != *" $required "* ]]; then
                printf 'fake docker: missing hardening flag %s\n' "$required" >&2
                exit 64
            fi
        done
        root="$state/containers/$name"
        mkdir -p "$root"
        [[ -z $label ]] || touch "$root/attempt-${label#tyrion.attempt=}"
        printf '%s\n' "$network" >"$root/network"
        printf '%s\n' "${aliases[*]-}" >"$root/aliases"
        if ((detach)); then
            printf 'tyrion-relay-ready\n' >"$root/stderr"
            printf 'running\n' >"$root/state"
        fi
        printf '%s\n' "$name"
        ;;
    start)
        name=$1
        [[ -d $state/containers/$name ]]
        printf 'running\n' >"$state/containers/$name/state"
        printf '%s\n' "$name"
        ;;
    logs)
        name=$1
        cat "$state/containers/$name/stderr" >&2 2>/dev/null || true
        ;;
    inspect)
        format=
        kind=container
        while (($#)); do
            case "$1" in
                --format) format=$2; shift 2 ;;
                --type) kind=$2; shift 2 ;;
                *) break ;;
            esac
        done
        name=$1
        [[ $kind == container ]]
        if [[ ! -d $state/containers/$name ]]; then
            printf 'Error: No such container: %s\n' "$name" >&2
            exit 1
        fi
        case "$format" in
            '{{.Image}}') printf '%s\n' "$image_id" ;;
            *IPAddress*) printf '%s\n' "10.88.0.2" ;;
            '') printf '[]\n' ;;
            *) printf '\n' ;;
        esac
        ;;
    exec)
        workdir=
        while (($#)); do
            case "$1" in
                --interactive|-i) shift ;;
                --workdir) workdir=$2; shift 2 ;;
                --env) shift 2 ;;
                *) break ;;
            esac
        done
        name=$1
        shift
        root="$state/containers/$name"
        mkdir -p "$root"
        if [[ " $* " == *tyrion-containment-probe* ]]; then
            required_probe_terms=(
                '/sys/fs/cgroup/pids.max'
                '/sys/fs/cgroup/memory.max'
                '/sys/fs/cgroup/memory.swap.max'
                '/sys/fs/cgroup/cpu.max'
                'nproc'
                'df -Pk /sandbox'
                'CapEff'
                'NoNewPrivs'
                'Seccomp'
                'id -u'
                '/var/run/docker.sock'
                '/run/containerd/containerd.sock'
                '$HOME/.ssh'
                '$HOME/.aws'
                '$HOME/.config/gh'
                '$HOME/.codex'
                '$HOME/.claude'
                '$HOME/.pi'
                'OPENAI_API_KEY'
                'ANTHROPIC_API_KEY'
                'GEMINI_API_KEY'
                'XAI_API_KEY'
                'GROQ_API_KEY'
                'OPENROUTER_API_KEY'
                'AWS_ACCESS_KEY_ID'
                'GH_TOKEN'
                'GITHUB_TOKEN'
                'SSH_AUTH_SOCK'
                '/etc/tyrion-probe'
                '/sandbox/tyrion-probe'
                'tyrion-exec-probe'
                '/proc/self/mountinfo'
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
            printf '%s\n' "$!" >"$state/containers/$name/preflight-descendant.pid"
            printf '%s\n' 'containment-ok'
            exit 0
        fi
        # Tyrion downloads by streaming `cat` out of the sandbox, so the
        # malformed-transfer fixture corrupts the candidate here.
        if [[ -e $state/corrupt-candidate && " $* " == *"cat '/sandbox/candidate.bundle'"* ]]; then
            printf '%s\n' 'not a Git bundle'
            exit 0
        fi
        # The Worker image carries the harness binaries; the fixture keeps them
        # in the image directory a test populates.
        harness="$state/image/harness"
        mapped=()
        for argument in "$@"; do
            argument="${argument//\/opt\/tyrion\/harness/$harness}"
            mapped+=("${argument///sandbox/$root}")
        done
        if [[ -n $workdir ]]; then
            cd "${workdir///sandbox/$root}"
        fi
        if [[ -e $state/fail-integrated-verification && $name == tyrion-i-* && -n $workdir ]]; then
            printf '%s\n' 'simulated integrated verification failure' >&2
            exit 7
        fi
        if [[ -e $state/hold-candidate-verification && $name == tyrion-c-* && -n $workdir ]]; then
            printf '%s\n' "$$" >"$state/containers/$name/verification-descendant.pid"
            sleep 300
            exit 91
        fi
        exec env \
            TYRION_WORKSPACE_ROOT="$root" \
            TYRION_HARNESS_ROOT="$harness" \
            TYRION_FAKE_STATE="$state" \
            HOME="$root" \
            "${mapped[@]}"
        ;;
    rm)
        while [[ ${1:-} == --* ]]; do shift; done
        name=$1
        if [[ ! -d $state/containers/$name ]]; then
            printf 'Error: No such container: %s\n' "$name" >&2
            exit 1
        fi
        if [[ $name == tyrion-c-* && -f $state/containers/$name/verification-descendant.pid ]]; then
            rm -f "$state/hold-candidate-verification"
        fi
        for descendant in "$state/containers/$name"/*descendant.pid; do
            [[ -f $descendant ]] || continue
            kill "$(cat "$descendant")" 2>/dev/null || true
            printf '%s\n' 'descendant-terminated' >>"$log"
        done
        rm -rf "${state:?}/containers/$name"
        printf '%s\n' "$name"
        ;;
    ps)
        filter=
        while (($#)); do
            case "$1" in
                --filter) filter=$2; shift 2 ;;
                *) shift ;;
            esac
        done
        attempt=${filter#label=tyrion.attempt=}
        for container in "$state/containers"/*; do
            [[ -d $container ]] || continue
            if [[ -z $filter ]]; then
                basename "$container"
            elif [[ $filter == label=tyrion.attempt ]]; then
                compgen -G "$container/attempt-*" >/dev/null && basename "$container"
            elif [[ -f $container/attempt-$attempt ]]; then
                basename "$container"
            fi
        done
        ;;
    images)
        # images [--format ...] REPOSITORY: one "repository:tag" per line.
        repository=${*: -1}
        for image in "$state/images"/*; do
            [[ -f $image ]] || continue
            name=$(basename "$image")
            [[ $name == "$repository":* ]] && printf '%s\n' "$name"
        done
        ;;
    rmi)
        for name in "$@"; do
            rm -f "$state/images/$name"
        done
        ;;
    network)
        action=$1
        shift
        case "$action" in
            create)
                name=${*: -1}
                subnet=
                previous=
                for argument in "$@"; do
                    [[ $previous == --subnet ]] && subnet=$argument
                    previous=$argument
                done
                # Real Docker draws a network without --subnet from a small
                # predefined pool (about 30 on Docker Desktop) and refuses
                # an explicit subnet that overlaps one already in use.
                if [[ -z $subnet ]]; then
                    pool=$(cat "$state/address-pool-size" 2>/dev/null || echo 30)
                    drawn=$(find "$state/networks" -mindepth 1 -maxdepth 1 -type d ! -exec test -e '{}/subnet' ';' -print | wc -l)
                    if ((drawn >= pool)); then
                        printf '%s\n' 'Error response from daemon: all predefined address pools have been fully subnetted' >&2
                        exit 1
                    fi
                elif grep -qxF "$subnet" "$state"/networks/*/subnet 2>/dev/null; then
                    printf '%s\n' 'Error response from daemon: Pool overlaps with other one on this address space' >&2
                    exit 1
                fi
                mkdir -p "$state/networks/$name"
                [[ -z $subnet ]] || printf '%s\n' "$subnet" >"$state/networks/$name/subnet"
                previous=
                for argument in "$@"; do
                    [[ $previous == --label ]] && printf '%s\n' "$argument" >"$state/networks/$name/label"
                    previous=$argument
                done
                printf '%s\n' "$name"
                ;;
            connect) printf '\n' ;;
            rm)
                for name in "$@"; do
                    rm -rf "${state:?}/networks/$name"
                done
                ;;
            ls)
                # --filter label=KEY matches any value; label=KEY=VALUE only that one.
                filter=
                previous=
                for argument in "$@"; do
                    [[ $previous == --filter ]] && filter=${argument#label=}
                    previous=$argument
                done
                for network in "$state/networks"/*; do
                    [[ -f $network/label ]] || continue
                    label=$(cat "$network/label")
                    if [[ -z $filter || $label == "$filter" || ${label%%=*} == "$filter" ]]; then
                        basename "$network"
                    fi
                done
                ;;
            *) printf 'unsupported fake network action: %s\n' "$action" >&2; exit 2 ;;
        esac
        ;;
    *)
        printf 'unsupported fake docker operation: %s\n' "$operation" >&2
        exit 2
        ;;
esac
