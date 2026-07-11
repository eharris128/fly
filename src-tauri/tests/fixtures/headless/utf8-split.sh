#!/bin/sh
# Fake claude for the bytes-per-line guarantee: one result line far larger
# than the runner's 8 KiB read buffer, packed with multibyte characters so
# chunk boundaries land inside characters — and a rocket (U+1F680, 4 bytes)
# deliberately flushed in two separate writes split mid-character (octal
# escapes; each printf is its own process, hence its own write(2)).
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-utf8"}'
s=""
i=0
while [ $i -lt 800 ]; do
  s="${s}héllo☃世界—"
  i=$((i + 1))
done
printf '{"type":"result","subtype":"success","is_error":false,"result":"%s' "$s"
printf '\360\237'
sleep 0.2
printf '\232\200"}\n'
exit 0
