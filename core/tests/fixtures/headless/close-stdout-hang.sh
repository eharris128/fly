#!/bin/sh
# Fake claude that closes stdout with no result event and then hangs: the
# runner must kill immediately on EOF-no-result — never wait out the
# deadline. Pids are recorded (cwd) so the test can assert no survivors.
echo "diagnostic noise before dying"
echo $$ > pids.txt
exec 1>&-
sleep 100 &
echo $! >> pids.txt
wait
