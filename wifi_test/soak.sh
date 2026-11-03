#!/bin/sh
# Repeated transfers to and from wifi_test, each checked by SHA-1.
#
# Usage: soak.sh <board address> [rounds] [port]
#
# Each round downloads /data and compares it with the pattern the board sends, then uploads
# 1 MiB of new random bytes to /upload and compares the board's SHA-1 of them with its own.
# Needs curl, sha1sum and head. Exits nonzero if any transfer failed or differed.

set -u

address=${1:?usage: soak.sh <board address> [rounds] [port]}
rounds=${2:-100}
port=${3:-8080}
url="http://$address:$port"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The board repeats this 64-byte line; 16384 lines make 1 MiB.
line=0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.
expected=$(yes "$line" | head -n 16384 | sha1sum | cut -d' ' -f1)

failures=0
down_total=0
up_total=0

i=1
while [ "$i" -le "$rounds" ]; do
    # Download.
    if speed=$(curl -sS --max-time 30 -o "$work/down" -w '%{speed_download}' "$url/data"); then
        got=$(sha1sum "$work/down" | cut -d' ' -f1)
        if [ "$got" = "$expected" ]; then
            down="$((${speed%.*} / 1024)) KiB/s"
            down_total=$((down_total + 1))
        else
            down="WRONG DATA"
            failures=$((failures + 1))
        fi
    else
        down="FAILED"
        failures=$((failures + 1))
    fi

    # Upload.
    head -c 1048576 /dev/urandom > "$work/up"
    sent=$(sha1sum "$work/up" | cut -d' ' -f1)
    if answer=$(curl -sS --max-time 30 --data-binary @"$work/up" \
        -H 'Content-Type: application/octet-stream' "$url/upload"); then
        board=$(printf '%s\n' "$answer" | sed -n 's/^SHA-1 //p')
        if [ "$board" = "$sent" ]; then
            up=$(printf '%s\n' "$answer" | sed -n 's/.* ms, \([0-9]*\) KiB\/s$/\1 KiB\/s/p')
            up_total=$((up_total + 1))
        else
            up="WRONG DATA"
            failures=$((failures + 1))
        fi
    else
        up="FAILED"
        failures=$((failures + 1))
    fi

    echo "$i: down $down, up $up"
    i=$((i + 1))
done

echo "Downloads good: $down_total of $rounds. Uploads good: $up_total of $rounds. Failures: $failures"
[ "$failures" -eq 0 ]
