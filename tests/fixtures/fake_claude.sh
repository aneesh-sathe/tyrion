#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
  if [ -z "${TYRION_WORKSPACE_ROOT:-}" ]; then
    printf '%s\n' 'guest Claude was executed outside the sandbox' >&2
    exit 86
  fi
  printf '%s\n' '2.1.204 (Claude Code)'
  exit 0
fi

printf '%s\n' 'fake Claude only supports --version' >&2
exit 64
