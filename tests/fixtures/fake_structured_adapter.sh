#!/bin/sh
set -eu
IFS= read -r launch
case "$launch" in
  *'"type":"tyrion.assignment.launch"'*) ;;
  *) echo 'missing structured launch' >&2; exit 2 ;;
esac
kind=${1:?adapter kind is required}
case "$launch:$kind" in
  *'report malformed required Skill failure'*:claude)
    printf '%s\n' '{"type":"tyrion.adapter.unavailable","code":"required_skill_failure","skill":{"name":"code-review","content_digest":"sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"},"message":"malformed fixture report"}'
    exit 7
    ;;
  *'fail required Skill on preferred harness'*:claude)
    printf '%s\n' "$launch" | python3 -c '
import json, sys
launch = json.load(sys.stdin)
skill = next(item for item in launch["skill_defaults"] if item["requirement"] == "required")
print(json.dumps({
    "type": "tyrion.adapter.unavailable",
    "code": "required_skill_failure",
    "skill": {"name": skill["name"], "content_digest": skill["content_digest"]},
    "message": "Claude native Skill activation failed in the fixture",
}, separators=(",", ":")))
'
    exit 7
    ;;
esac
printf '%s\n' "$launch" | TYRION_FIXTURE_KIND="$kind" python3 -c '
import json, os, sys
launch = json.load(sys.stdin)
kind = os.environ["TYRION_FIXTURE_KIND"]
session = {
    "codex": "codex-thread-fixture",
    "claude": "claude-session-fixture",
    "pi": "pi-session-fixture",
}[kind]
inventory = launch["worker_configuration"].get("skills", [])
names = sorted(skill["name"] if isinstance(skill, dict) else skill for skill in inventory)
preparations = [
    {
        "name": skill["name"],
        "content_digest": skill["content_digest"],
        "source": "fixture",
    }
    for skill in launch.get("skill_defaults", [])
]
print(json.dumps({
    "type": "tyrion.adapter.ready",
    "native_session_id": session,
    "native_skills": names,
    "native_skill_preparations": preparations,
    "configuration_fingerprint": os.environ["TYRION_CONFIGURATION_FINGERPRINT"],
}, separators=(",", ":")))
for skill in preparations:
    print(json.dumps({"type": "tyrion.skill.invoked", **skill}, separators=(",", ":")))
if "invoke native Worker-selected Skill" in launch["goal"]:
    skill = next(skill for skill in inventory if skill["name"] == "frontend")
    print(json.dumps({
        "type": "tyrion.skill.invoked",
        "name": skill["name"],
        "content_digest": skill["content_digest"],
        "source": "fixture",
    }, separators=(",", ":")))
'
case "$launch" in
  *'invoke native Worker-selected Skill then exit nonzero'*)
    exit 9
    ;;
esac
if [ "$kind" = codex ]; then
  printf '%s\n' \
    '{"method":"turn/started","params":{"turn":{"status":"inProgress"}}}'
elif [ "$kind" = pi ]; then
  printf '%s\n' '{"type":"agent_start"}'
else
  printf '%s\n' \
    '{"type":"session.status_running"}'
fi
if [ -n "${TYRION_CANDIDATE_BUNDLE:-}" ]; then
  worktree=$(mktemp -d /tmp/tyrion-structured-git.XXXXXX)
  git clone -q -b tyrion-base "$TYRION_BASE_BUNDLE" "$worktree/repository"
  git -C "$worktree/repository" config user.name 'Tyrion Structured Fixture'
  git -C "$worktree/repository" config user.email 'structured-fixture@tyrion.invalid'
  if [ "$kind" = codex ]; then
    printf '%s\n' backend >"$worktree/repository/backend.txt"
    git -C "$worktree/repository" add backend.txt
    git -C "$worktree/repository" commit -qm 'feat: add backend artifact'
  else
    printf '%s\n' frontend >"$worktree/repository/frontend.txt"
    git -C "$worktree/repository" add frontend.txt
    git -C "$worktree/repository" commit -qm 'feat: add frontend artifact'
  fi
  git -C "$worktree/repository" branch tyrion-result
  git -C "$worktree/repository" bundle create "$TYRION_CANDIDATE_BUNDLE" refs/heads/tyrion-result
fi
case "$launch" in
  *'spawn structured descendant'*)
    sleep 300 &
    printf '%s\n' "$!" >"${TYRION_WORKSPACE_ROOT:?}/adapter-descendant.pid"
    ;;
esac
case "$launch" in
  *'break structured control pipe'*)
    exec 0<&-
    : >"${TYRION_FAKE_STATE:?}/control-pipe-closed"
    sleep 1
    ;;
esac
case "$launch" in
  *'report structured failure'*)
    if [ "$kind" = codex ]; then
      printf '%s\n' \
        '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":6,"outputTokens":1}}}}' \
        '{"method":"error","params":{"message":"fixture failure"}}'
    elif [ "$kind" = claude ]; then
      printf '%s\n' \
        '{"type":"span.model_request_end","usage":{"input_tokens":6,"output_tokens":1}}' \
        '{"type":"session.error","message":"fixture failure"}'
    else
      printf '%s\n' \
        '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":6,"output":1,"cost":{"total":0}}}}' \
        '{"type":"tyrion.pi.usage","input_tokens":23,"output_tokens":7,"cost":0}' \
        '{"type":"extension_error","error":"fixture failure"}' \
        '{"type":"agent_settled"}'
    fi
    exit 0
    ;;
esac
case "$launch" in
  *'hold for structured control'*)
    IFS= read -r steer
    case "$steer" in *'"type":"tyrion.worker.steer"'*) ;; *) exit 3 ;; esac
    IFS= read -r interrupt
    case "$interrupt" in *'"type":"tyrion.worker.interrupt"'*) ;; *) exit 4 ;; esac
    if [ "$kind" = codex ]; then
      printf '%s\n' \
        '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":2,"outputTokens":0}}}}' \
        '{"method":"turn/completed","params":{"turn":{"status":"interrupted"}}}'
    elif [ "$kind" = claude ]; then
      printf '%s\n' \
        '{"type":"span.model_request_end","usage":{"input_tokens":2,"output_tokens":0}}' \
        '{"type":"user.interrupt"}' \
        '{"type":"session.status_idle"}'
    else
      printf '%s\n' \
        '{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":2,"output":0,"cost":{"total":0}}}}' \
        '{"type":"tyrion.pi.interrupt"}' \
        '{"type":"tyrion.pi.usage","input_tokens":19,"output_tokens":4,"cost":0}' \
        '{"type":"agent_settled"}'
    fi
    exit 0
    ;;
esac
if [ "$kind" = codex ]; then
  printf '%s\n' \
    '{"method":"item/completed","params":{"item":{"type":"agentMessage","text":"fixture completed"}}}' \
    '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":12,"outputTokens":4}}}}' \
    '{"method":"turn/completed","params":{"turn":{"status":"completed"}}}'
elif [ "$kind" = claude ]; then
  printf '%s\n' \
    '{"type":"agent.message","content":[{"type":"text","text":"fixture completed"}]}' \
    '{"type":"span.model_request_end","usage":{"input_tokens":10,"output_tokens":5}}' \
    '{"type":"session.status_idle"}'
else
  printf '%s\n' \
    '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"fixture completed"}],"usage":{"input":11,"output":6,"cost":{"total":0}}}}' \
    '{"type":"tyrion.pi.usage","input_tokens":11,"output_tokens":6,"cost":0}' \
    '{"type":"agent_settled"}'
fi
printf '{"type":"tyrion.result","commission_id":"%s","assignment_id":"%s","attempt_id":"%s","mandate_revision":%s,"plan_revision":%s,"summary":"return a routed greeting","known_effects":[],"cost_cents":0}\n' \
  "$TYRION_COMMISSION_ID" "$TYRION_ASSIGNMENT_ID" "$TYRION_ATTEMPT_ID" \
  "$TYRION_MANDATE_REVISION" "$TYRION_PLAN_REVISION"
