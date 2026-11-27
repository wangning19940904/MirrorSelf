#!/bin/sh
# A synthetic command-line source for tests. Prints the file named by
# FAKE_CLI_OUTPUT, and appends its arguments to FAKE_CLI_LOG.
#   FAKE_CLI_FAIL=1  exit 3 with a message on stderr instead
[ -n "$FAKE_CLI_LOG" ] && printf '%s\n' "$*" >> "$FAKE_CLI_LOG"
if [ "$FAKE_CLI_FAIL" = 1 ]; then
  echo "calendar backend unavailable (synthetic failure)" >&2
  exit 3
fi
cat "$FAKE_CLI_OUTPUT"
