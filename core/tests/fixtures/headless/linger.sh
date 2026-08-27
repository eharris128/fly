#!/bin/sh
# Fake claude exhibiting the observed backgrounding quirk: a success result
# streamed early, then the process lingers (stdout held open) far past any
# linger grace. The runner must kill it (LingerKilled) and still close
# Clean, bounded fast.
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-linger"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"done; task still running","session_id":"s-linger"}'
sleep 100
