#!/usr/bin/env bash
# Play something, by voice.
#
# All this does now is hand the query to the daemon and report what came
# back. It used to resolve the search, start mpv in the background and tell
# the overlay what was playing over a separate now-playing channel -- which
# meant nothing could pause, skip or queue it, because the process was started
# and forgotten. The daemon owns the player now and does all of that, so the
# script is a voice-shaped front door onto `arc music play`.
#
# The one thing it still decides is what to say. The daemon's answer carries
# the real title from the resolver, and saying that back is the point: "playing
# Hall of Fame by Boards of Canada" is useful, echoing the query is not.
set -uo pipefail

ARC="${ARC_COMMAND:-arc}"

QUERY="${ARC_ARG_QUERY:-}"
if [ -z "$QUERY" ]; then
  echo "ERROR: no query given"
  exit 1
fi

# `--json` so the reply is parsed rather than scraped off stdout: the plain
# output is written for a person, and this is a program.
OUT=$("$ARC" --json music play "$QUERY" 2>&1)
RC=$?

if [ $RC -ne 0 ]; then
  # The daemon reports a failed search as its message plus, when it can, that a
  # browser page was opened instead. Both are worth saying out loud: silence
  # after a request to play music reads as a bug.
  MSG=$(printf '%s' "$OUT" | tr '\n' ' ')
  echo "ERROR: ${MSG:-could not play that}"
  exit 1
fi

LABEL=$(printf '%s' "$OUT" | sed -n 's/.*"label"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
if [ -z "$LABEL" ]; then
  echo "ERROR: the daemon accepted the request but named no track"
  exit 1
fi

QUEUED=$(printf '%s' "$OUT" | grep -o '"queue"[[:space:]]*:[[:space:]]*\[[^]]*\]' | grep -o '{' | wc -l | tr -d ' ')
if [ "${QUEUED:-0}" -gt 0 ]; then
  echo "Playing $LABEL — $QUEUED more queued"
else
  echo "Playing $LABEL"
fi