#!/bin/sh
# Fake claude gone wrong: only unparsable output, then a clean exit —
# EOF with no result event (R11) must classify Infra.
echo "not json at all"
echo "}{ definitely not json"
exit 0
