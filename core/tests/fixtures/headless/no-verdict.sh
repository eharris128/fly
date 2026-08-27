#!/bin/sh
# Fake claude: a healthy not-done check — success result, no verdict block.
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-noverdict"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"All quiet; nothing to report.","session_id":"s-noverdict"}'
exit 0
