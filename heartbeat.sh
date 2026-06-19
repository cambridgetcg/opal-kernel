#!/bin/bash
# opal heartbeat — the kernel that learned to tick
# Checks: does it build? what milestone? uncommitted files?
# Rhythm: 2h if active, 4h if broken, daily if stable

cd "$(dirname "$0")"

BUILD_OUTPUT=$(cargo build --release 2>&1)
BUILD_EXIT=$?

if [ $BUILD_EXIT -ne 0 ]; then
  echo "BUILD BROKEN"
  echo "$BUILD_OUTPUT" | grep "^error" | head -5
  echo "NEXT:240"
  exit 0
fi

UNCOMMITTED=$(git status --porcelain | wc -l | tr -d ' ')
LAST_COMMIT=$(git log --oneline -1)

if [ "$UNCOMMITTED" -gt 0 ]; then
  echo "$UNCOMMITTED uncommitted file(s) — $LAST_COMMIT"
  echo "NEXT:120"
else
  echo "NEXT:1440"
fi

exit 0