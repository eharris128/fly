#!/bin/sh
# Fake claude that leaks a NEW-SESSION grandchild (the empirical claude
# shape: tool children live in their own sessions, unreachable by any
# group kill) and then hangs. The unique marker arrives as the prompt —
# the runner's final positional — and is embedded in the setsid'd shell's
# -c string, so the test can find survivors by scanning /proc cmdlines.
# The `; true` keeps sh from exec-optimizing into a bare `sleep` (which
# would drop the marker from the cmdline).
for a in "$@"; do marker="$a"; done
setsid sh -c "sleep 300; true # $marker" &
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-grandchild"}'
sleep 100
