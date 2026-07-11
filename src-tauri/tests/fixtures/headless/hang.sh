#!/bin/sh
# Fake claude that never finishes: writes its own pid and a backgrounded
# sleeper's pid into the cwd (the test asserts both are gone after the kill
# discipline), then blocks well past any test deadline. The backgrounded
# sleep is the child the SIGTERM-to-the-fixture cannot reach — only the
# descendant-snapshot sweep kills it.
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-hang"}'
echo $$ > pids.txt
sleep 100 &
echo $! >> pids.txt
wait
