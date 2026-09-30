#!/usr/bin/env bash
# Play a track, and tell Arc what is playing so the overlay can show it.
#
# The overlay has no way to know a track started on its own: mpv is launched
# here in the background and this script exits immediately. So it reports
# through the daemon's now-playing IPC (`arc --json now-playing set ...`),
# passing mpv's pid. The daemon watches that pid and clears the overlay's row
# when the process is gone -- that is what makes the row disappear when the
# track ends rather than sticking around until the next request.
#
# Report only on success. A track that never started must not leave a title on
# screen, so every failure path clears the row instead of setting it.
set -uo pipefail

ARC="${ARC_COMMAND:-arc}"

report_stop() {
  # Never let reporting fail the playback: the music plays either way.
  "$ARC" --json now-playing stop >/dev/null 2>&1 || true
}

report_playing() {
  local title="$1" artist="$2" pid="$3"
  "$ARC" --json now-playing set --title "$title" --artist "$artist" \
    --source "youtube music" --pid "$pid" >/dev/null 2>&1 || true
}

QUERY="${ARC_ARG_QUERY:-}"
if [ -z "$QUERY" ]; then
  report_stop
  echo "ERROR: no query given"
  exit 1
fi

ENC=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1]))' "$QUERY" 2>/dev/null)
URL="https://music.youtube.com/search?q=${ENC:-$QUERY}"

# Preferred: stream it locally so it actually plays.
if command -v yt-dlp >/dev/null 2>&1 && command -v mpv >/dev/null 2>&1; then
  # Metadata first, so the overlay shows the real title and artist rather than
  # echoing the search query back at the user. Two cheap --print calls rather
  # than -J, which downloads the whole metadata document to read two fields.
  TITLE=$(yt-dlp --skip-download --print "%(title)s" "ytsearch1:$QUERY" 2>/dev/null | head -n1)
  ARTIST=$(yt-dlp --skip-download --print "%(uploader)s" "ytsearch1:$QUERY" 2>/dev/null | head -n1)
  STREAM=$(yt-dlp -f bestaudio -g "ytsearch1:$QUERY" 2>/dev/null | head -n1)
  if [ -n "$STREAM" ]; then
    mpv --no-video --really-quiet "$STREAM" >/dev/null 2>&1 &
    MPV_PID=$!
    # yt-dlp's fields can be "NA" for a track with no uploader, which is worse
    # than saying nothing; the query is a better fallback than that.
    if [ -z "$TITLE" ] || [ "$TITLE" = "NA" ]; then TITLE="$QUERY"; fi
    if [ "$ARTIST" = "NA" ]; then ARTIST=""; fi
    report_playing "$TITLE" "$ARTIST" "$MPV_PID"
    echo "Playing locally with mpv: $TITLE${ARTIST:+ — $ARTIST}"
    exit 0
  fi
  # No report here: the fallback below clears the row on its way out, and
  # clearing twice is one redundant IPC round trip for nothing.
  echo "yt-dlp found no stream for: $QUERY"
fi

# Fallback: hand the search page to the browser. There is no pid to watch and
# nothing Arc started, so the row is cleared rather than left claiming a track
# the browser is not playing.
report_stop
if command -v xdg-open >/dev/null 2>&1; then
  xdg-open "$URL" >/dev/null 2>&1 &
  echo "Opened YouTube Music search in browser for: $QUERY ($URL)"
  echo "Note: you may need to click the top result yourself."
  exit 0
fi

echo "ERROR: neither mpv/yt-dlp nor xdg-open available, cannot play anything"
exit 1