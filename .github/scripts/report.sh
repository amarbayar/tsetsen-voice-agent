#!/usr/bin/env bash
# Runs a command; if it fails, posts the end of its output as an annotation
# on the run, so the cause is readable without opening the full log.
set -o pipefail
log="$(mktemp)"
"$@" 2>&1 | tee "$log"
status=$?
if [[ $status -ne 0 ]]; then
  body="$(grep -vE '^\s*(Compiling|Checking|Downloaded|Downloading|Fresh|Blocking)' "$log" | tail -n 60 \
    | awk 'BEGIN { ORS = "%0A" } { gsub(/%/, "%25"); gsub(/\r/, ""); print }')"
  echo "::error title=$1 $2 failed::$body"
fi
exit $status
