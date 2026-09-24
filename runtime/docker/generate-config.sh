#!/bin/bash
# Generate a Tyrion Worker runtime configuration and catalog for this machine.
#
# Every value Tyrion verifies at startup is discovered here rather than typed:
# binary hashes, the Docker CLI version, the Worker image identity, and the
# harness versions. A wrong digest makes tyriond refuse to start, which is
# correct but opaque, so the point of this script is that you never write one
# by hand.
#
#   runtime/docker/generate-config.sh --image tyrion-worker:TAG --out DIR \
#     [--claude PATH] [--codex PATH] [--codex-code-mode-host PATH]
#
# At least one harness binary is required, and they must be the Linux build for
# your image's architecture: they run inside the container, never on the host.
set -euo pipefail

IMAGE=""; OUT=""; CLAUDE=""; CODEX=""; CMH=""
DOCKER_BIN="$(command -v docker || true)"
DOCKER_HOST_ADDR="${DOCKER_HOST:-unix://$HOME/.docker/run/docker.sock}"

while (($#)); do
  case "$1" in
    --image) IMAGE=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --claude) CLAUDE=$2; shift 2 ;;
    --codex) CODEX=$2; shift 2 ;;
    --codex-code-mode-host) CMH=$2; shift 2 ;;
    --docker-binary) DOCKER_BIN=$2; shift 2 ;;
    --docker-host) DOCKER_HOST_ADDR=$2; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 64 ;;
  esac
done

die(){ echo "error: $*" >&2; exit 1; }
[ -n "$IMAGE" ] || die "--image is required (build it with runtime/docker/Dockerfile)"
[ -n "$OUT" ] || die "--out is required"
[ -n "$DOCKER_BIN" ] || die "docker not found; pass --docker-binary"
[ -n "$CLAUDE$CODEX" ] || die "give at least one of --claude or --codex"

sha(){ shasum -a 256 "$1" | cut -d' ' -f1; }
need_file(){ [ -f "$1" ] || die "$2 is not a file: $1"; }

need_file "$DOCKER_BIN" "docker binary"
IMAGE_ID=$("$DOCKER_BIN" image inspect "$IMAGE" --format '{{.Id}}' 2>/dev/null) \
  || die "image '$IMAGE' is not present locally. Tyrion never pulls; build or pull it first."
DOCKER_VERSION=$("$DOCKER_BIN" --version)

# A guest-only Linux binary cannot report its version on the host, so ask the
# container. This is also the first proof the binary runs under the profile.
guest_version(){
  "$DOCKER_BIN" run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges --security-opt seccomp=builtin --user 65534:65534 \
    --tmpfs /sandbox:rw,exec,nosuid,nodev,size=1024m,mode=1777 \
    -e HOME=/sandbox -v "$1:/opt/harness:ro" "$IMAGE" \
    sh -c 'cp /opt/harness /sandbox/h && chmod 700 /sandbox/h && /sandbox/h --version' 2>/dev/null | tail -1
}

mkdir -p "$OUT"
CLAUDE_BLOCK=null; CODEX_BIN=""; CODEX_SHA=""; CODEX_VER=""; CMH_BLOCK=null
DESTS='{"host":"api.anthropic.com","port":443}'

if [ -n "$CLAUDE" ]; then
  need_file "$CLAUDE" "claude binary"
  V=$(guest_version "$CLAUDE"); [ -n "$V" ] || die "claude binary did not run in $IMAGE (wrong architecture?)"
  echo "claude : $V"
  CLAUDE_BLOCK=$(printf '{"binary":"%s","version":"%s","sha256":"%s"}' "$CLAUDE" "$V" "$(sha "$CLAUDE")")
fi

if [ -n "$CODEX" ]; then
  need_file "$CODEX" "codex binary"
  V=$(guest_version "$CODEX"); [ -n "$V" ] || die "codex binary did not run in $IMAGE (wrong architecture?)"
  echo "codex  : $V"
  CODEX_BIN=$CODEX; CODEX_SHA=$(sha "$CODEX"); CODEX_VER=$V
  # Codex delegates edits and shell work to this helper and fails without it.
  [ -n "$CMH" ] || die "--codex-code-mode-host is required with --codex"
  need_file "$CMH" "codex code-mode host"
  CMH_BLOCK=$(printf '{"path":"%s","sha256":"%s"}' "$CMH" "$(sha "$CMH")")
  DESTS="$DESTS,"'{"host":"chatgpt.com","port":443},{"host":"auth.openai.com","port":443}'
fi

# Codex needs a stub even when unused, because the runtime pins it unconditionally.
if [ -z "$CODEX_BIN" ]; then
  CODEX_BIN="$OUT/codex-unused"
  printf '#!/bin/sh\nexit 1\n' > "$CODEX_BIN"; chmod 700 "$CODEX_BIN"
  CODEX_SHA=$(sha "$CODEX_BIN"); CODEX_VER="codex-cli 0.156.1"
fi

CODEX_AUTH=""
[ -f "$HOME/.codex/auth.json" ] && CODEX_AUTH="$HOME/.codex/auth.json"

python3 - "$OUT" "$DOCKER_BIN" "$(sha "$DOCKER_BIN")" "$DOCKER_VERSION" "$DOCKER_HOST_ADDR" \
         "$IMAGE_ID" "$CODEX_BIN" "$CODEX_SHA" "$CODEX_VER" "$CLAUDE_BLOCK" "$CMH_BLOCK" \
         "$DESTS" "$CODEX_AUTH" <<'PY'
import json, pathlib, sys
(out, dbin, dsha, dver, dhost, image_id, cbin, csha, cver,
 claude, cmh, dests, codex_auth) = sys.argv[1:14]
runtime = {
  "docker_binary": dbin, "docker_sha256": dsha, "docker_version": dver,
  "docker_host": dhost,
  # A locally built image has no registry digest, only an id. Both are content
  # addressed; a tag is not, and Tyrion rejects one.
  "worker_image": image_id, "worker_image_id": image_id,
  "codex_binary": cbin, "codex_sha256": csha, "codex_version": cver,
  "model": "gpt-5.6-sol",
  "egress": {"destinations": json.loads("[" + dests + "]")},
  "worker_credentials": [],
  "lease_ttl_seconds": 900, "vcpus": 2, "memory_mib": 6144,
  "writable_storage_mib": 4096, "max_processes": 256,
}
if claude != "null":
    runtime["claude"] = json.loads(claude)
if cmh != "null":
    runtime["codex_code_mode_host"] = json.loads(cmh)
if codex_auth:
    runtime["codex_auth_file"] = codex_auth
pathlib.Path(out, "codex-worker.json").write_text(json.dumps(runtime, indent=2) + "\n")
print("\nwrote", pathlib.Path(out, "codex-worker.json"))
if not codex_auth:
    print("note : no ~/.codex/auth.json found; run `codex login` before using Codex Workers")
print("note : set worker_credentials to the env var names a harness needs,")
print("       for example CLAUDE_CODE_OAUTH_TOKEN from `claude setup-token`")
PY
