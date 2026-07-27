#!/bin/sh

set -eu

binary=$1
shift

identity="Dakia Local Development"
identifier="dev.dakia.mail"

if [ "$(uname -s)" = "Darwin" ] && \
  /usr/bin/security find-identity -v -p codesigning 2>/dev/null | \
    /usr/bin/grep -Fq "\"$identity\""; then
  /usr/bin/codesign \
    --force \
    --options runtime \
    --identifier "$identifier" \
    --sign "$identity" \
    --timestamp=none \
    "$binary"
fi

exec "$binary" "$@"
