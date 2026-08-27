#!/bin/sh
# Fake claude: the captured real stream shape ending in a success result
# whose text carries a PASS ```verdict fence. Verdict parsing is U4's — the
# U3 tests assert only the exact text + session id reaching the closer.
# printf '%s\n' keeps the JSON's \n escapes literal (two chars) so the
# stream stays one JSON object per line.
printf '%s\n' '{"type":"system","subtype":"init","cwd":"/tmp","session_id":"11111111-2222-3333-4444-555555555555","model":"claude-sonnet-4-5","permissionMode":"bypassPermissions"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Checking the experiment now."}]}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"Experiment done.\n\n```verdict\nstatus: PASS\nsummary: converged\n```","session_id":"11111111-2222-3333-4444-555555555555"}'
exit 0
