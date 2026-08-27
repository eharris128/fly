#!/bin/sh
# Fake claude for the R13 hygiene refactor-guard: dumps its env, cwd, and
# full argv into the automation cwd, then emits a minimal clean stream.
env > env-dump.txt
pwd -P > cwd-dump.txt
printf '%s\n' "$@" > argv-dump.txt
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-env"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"env dumped"}'
exit 0
