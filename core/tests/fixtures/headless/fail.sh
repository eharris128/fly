#!/bin/sh
# Fake claude crashing: stderr noise, nonzero exit, no result event.
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-fail"}'
echo "boom: config missing" >&2
exit 3
