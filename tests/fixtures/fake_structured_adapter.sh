#!/bin/sh
set -eu
IFS= read -r launch
case "$launch" in
  *'"type":"tyrion.assignment.launch"'*) ;;
  *) echo 'missing structured launch' >&2; exit 2 ;;
esac
kind=${1:?adapter kind is required}
if [ "$kind" = codex ]; then
  printf '%s\n' \
    "{\"type\":\"tyrion.adapter.ready\",\"native_session_id\":\"codex-thread-fixture\",\"native_skills\":[\"code-review\",\"backend\",\"frontend\"],\"native_skill_outcomes\":[{\"name\":\"code-review\",\"outcome\":\"activated\",\"source\":\"fixture\"},{\"name\":\"backend\",\"outcome\":\"activated\",\"source\":\"fixture\"},{\"name\":\"frontend\",\"outcome\":\"activated\",\"source\":\"fixture\"}],\"configuration_fingerprint\":\"$TYRION_CONFIGURATION_FINGERPRINT\"}" \
    '{"method":"turn/started","params":{"turn":{"status":"inProgress"}}}'
else
  printf '%s\n' \
    "{\"type\":\"tyrion.adapter.ready\",\"native_session_id\":\"claude-session-fixture\",\"native_skills\":[\"code-review\",\"backend\",\"frontend\"],\"native_skill_outcomes\":[{\"name\":\"code-review\",\"outcome\":\"activated\",\"source\":\"fixture\"},{\"name\":\"backend\",\"outcome\":\"activated\",\"source\":\"fixture\"},{\"name\":\"frontend\",\"outcome\":\"activated\",\"source\":\"fixture\"}],\"configuration_fingerprint\":\"$TYRION_CONFIGURATION_FINGERPRINT\"}" \
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
    else
      printf '%s\n' \
        '{"type":"span.model_request_end","usage":{"input_tokens":6,"output_tokens":1}}' \
        '{"type":"session.error","message":"fixture failure"}'
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
    else
      printf '%s\n' \
        '{"type":"span.model_request_end","usage":{"input_tokens":2,"output_tokens":0}}' \
        '{"type":"user.interrupt"}' \
        '{"type":"session.status_idle"}'
    fi
    exit 0
    ;;
esac
if [ "$kind" = codex ]; then
  printf '%s\n' \
    '{"method":"item/completed","params":{"item":{"type":"agentMessage","text":"fixture completed"}}}' \
    '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":12,"outputTokens":4}}}}' \
    '{"method":"turn/completed","params":{"turn":{"status":"completed"}}}'
else
  printf '%s\n' \
    '{"type":"agent.message","content":[{"type":"text","text":"fixture completed"}]}' \
    '{"type":"span.model_request_end","usage":{"input_tokens":10,"output_tokens":5}}' \
    '{"type":"session.status_idle"}'
fi
printf '{"type":"tyrion.result","commission_id":"%s","assignment_id":"%s","attempt_id":"%s","mandate_revision":%s,"plan_revision":%s,"summary":"return a routed greeting","known_effects":[]}\n' \
  "$TYRION_COMMISSION_ID" "$TYRION_ASSIGNMENT_ID" "$TYRION_ATTEMPT_ID" \
  "$TYRION_MANDATE_REVISION" "$TYRION_PLAN_REVISION"
