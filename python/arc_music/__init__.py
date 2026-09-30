"""Arc's YouTube Music API resolver.

YouTube Music's own search is a better answer to "play aphex twin" than a
generic video search: it returns the studio track rather than a 40-minute
mix, and it comes back with the album, the length and real cover art. What
it does *not* return is anything playable -- a search result is a video id,
not a stream, and `ytmusicapi` has no way to turn one into a URL.

So this module does the half only the API can do (search, and the metadata
that goes with it) and hands each video id to yt-dlp, which does the half
only yt-dlp can do (resolve a playable stream). The daemon owns the player,
so it needs a URL; the API is used for what it is good at, not as a stand-in
for something it cannot do.

Speaks JSON lines on stdout, one object per result, in the same field names
the Rust side reads: title, artist, album, duration, video_id, url,
artwork, source. Errors go to stderr with a nonzero exit, because a blank
stdout is what "nothing found" looks like and the two must not be confused.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from typing import Any, Iterator

# `ytmusicapi` pulls in pandas, so it is imported lazily -- inside the search
# path rather than at module scope. Importing this module to read a constant
# should not cost a pandas import.
_SOURCE = "youtube music"

# yt-dlp's own format selector: bestaudio, and never a video stream. The
# player has no video output, so asking for one wastes bandwidth on a
# connection that mpv then has to throw away.
_YT_DLP_FORMAT = "bestaudio/best"

# One second per result, so a search that cannot be resolved fails fast
# instead of holding the daemon's socket handler for the full budget.
_STREAM_TIMEOUT_S = 20


class ResolveError(RuntimeError):
    """A search or a stream resolution failed. The message is user-facing."""


def _client():
    """A `YTMusic` client, or an error saying why there isn't one.

    Constructing the client is what reads the config file and does the auth
    handshake, so this is where "ytmusicapi is not installed" surfaces --
    rather than as an ImportError traceback from three frames deeper.
    """
    try:
        from ytmusicapi import YTMusic
    except ImportError as e:  # pragma: no cover - depends on the host
        raise ResolveError(
            "the ytmusicapi module is not installed "
            "(run: uv pip install --python ~/.local/share/arc/venv/bin/python ytmusicapi)"
        ) from e
    try:
        return YTMusic()
    except Exception as e:
        # A corrupt auth file is the usual cause and it names itself badly,
        # so say which file rather than passing the raw message up.
        raise ResolveError(f"could not open a YouTube Music session: {e}") from e


def _yt_dlp_binary() -> str:
    binary = shutil.which("yt-dlp")
    if not binary:
        raise ResolveError("yt-dlp is not on PATH, so a video id cannot be turned into a stream")
    return binary


def _stream_url(video_id: str) -> str:
    """The playable audio url for one YouTube video id.

    Deliberately not a batch: `--get-url` on several ids at once returns them
    as separate lines in an order that is not guaranteed to match the input,
    and a queue whose third track is the second search result is worse than
    no queue. One call per result, in order.
    """
    url = f"https://music.youtube.com/watch?v={video_id}"
    try:
        proc = subprocess.run(
            [
                _yt_dlp_binary(),
                "--no-config",
                "--no-warnings",
                "--no-playlist",
                "--socket-timeout",
                "10",
                "-f",
                _YT_DLP_FORMAT,
                "--get-url",
                url,
            ],
            capture_output=True,
            text=True,
            timeout=_STREAM_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired as e:
        raise ResolveError(f"yt-dlp did not answer in {_STREAM_TIMEOUT_S}s") from e
    if proc.returncode != 0:
        first = (proc.stderr or proc.stdout).strip().splitlines()
        detail = first[-1][:200] if first else f"exit {proc.returncode}"
        raise ResolveError(f"yt-dlp could not resolve {video_id}: {detail}")
    lines = [line.strip() for line in (proc.stdout or "").splitlines() if line.strip()]
    if not lines:
        raise ResolveError(f"yt-dlp returned no stream for {video_id}")
    return lines[0]


def _artist_text(result: dict[str, Any]) -> str:
    """The artist line, flattened from the API's list-of-objects shape.

    The API returns `[{"name": ..., "id": ...}]`, which JSON-serialises fine
    but reads as noise in a queue row. Several artists are joined with a
    comma rather than picking one: "Daft Punk, Pharrell Williams" is a
    feature, not an ambiguity.
    """
    artists = result.get("artists") or []
    names = [str(a.get("name", "")).strip() for a in artists if isinstance(a, dict)]
    return ", ".join(n for n in names if n)


def _artwork_url(result: dict[str, Any], size: int) -> str:
    """The largest cover art at or below `size` pixels wide.

    The API returns the same image at several sizes, smallest first, so
    walking the list and returning the first entry that fits would hand back
    the *smallest* one -- a 60px thumbnail blown up to fill a 220px card.
    The whole list is walked and the widest fitting entry wins.

    An entry with no usable width is still a usable image, so it counts as a
    candidate at the bottom of the order rather than being discarded. A
    playlist hit has no album and therefore no thumbnail at all, which is an
    empty string rather than an error: the track is still playable.
    """
    thumbs = result.get("thumbnails") or []
    best = ""
    best_width = -1
    for t in thumbs:
        if not isinstance(t, dict):
            continue
        url = str(t.get("url", "")).strip()
        if not url:
            continue
        width = t.get("width")
        if not isinstance(width, int):
            width = -1
        if width > size:
            # Too big to use as asked for; kept only as a last resort, in
            # case every entry is too big (which is better than no art).
            continue
        if width > best_width:
            best, best_width = url, width
    # Nothing fitted, which happens when the caller asks for less than the
    # smallest thumbnail on offer. The smallest is then the right answer --
    # scaling *up* from the nearest size beats downloading the original, and
    # beats showing no art at all.
    return best if best_width >= 0 else _smallest_thumb(thumbs)


def _smallest_thumb(thumbs: list[Any]) -> str:
    """The narrowest thumbnail, for when the request is smaller than all."""
    best, best_width = "", -1
    for t in thumbs:
        if not isinstance(t, dict):
            continue
        url = str(t.get("url", "")).strip()
        width = t.get("width")
        if url and isinstance(width, int) and (best_width < 0 or width < best_width):
            best, best_width = url, width
    return best


def _duration(result: dict[str, Any]) -> int:
    seconds = result.get("duration_seconds")
    return seconds if isinstance(seconds, int) and seconds > 0 else 0


def search(query: str, count: int, artwork_size: int = 544) -> Iterator[dict[str, Any]]:
    """Yield up to `count` playable results for `query`, best match first.

    Streams rather than returning a list: the daemon takes the first result,
    and building a list first would pay for stream resolution on results
    nobody asked for.

    Split into a validating outer function and an inner generator, because a
    bare `yield` in this body would defer the empty-query check and the API
    call until the first `next()` -- so `search("")` would look like it
    succeeded and only fail when someone tried to read the result. The eager
    half raises where the call is; the lazy half still skips resolving
    results nobody will read.
    """
    query = query.strip()
    if not query:
        raise ResolveError("nothing to search for")

    client = _client()
    try:
        # `songs` rather than the default: the default mix of videos, shorts
        # and episodes is why a music request used to come back with a live
        # cover. Songs is the only filter that is always the studio track.
        results = client.search(query, filter="songs", limit=max(1, min(count, 50)))
    except Exception as e:
        raise ResolveError(f"YouTube Music search failed: {e}") from e

    return _resolve_results(query, results or [], count, artwork_size)


def _resolve_results(
    query: str, results: list[Any], count: int, artwork_size: int
) -> Iterator[dict[str, Any]]:
    """The lazy half of [`search`]: video ids in, playable rows out.

    `query` is passed in rather than closed over -- it is only needed for the
    failure message, and threading it through keeps the two halves honestly
    separable instead of relying on the outer frame.
    """
    yielded = 0
    skipped: list[str] = []
    for result in results:
        if yielded >= count:
            break
        if not isinstance(result, dict):
            continue
        title = str(result.get("title", "")).strip()
        video_id = str(result.get("videoId", "")).strip()
        if not title or not video_id:
            continue
        try:
            url = _stream_url(video_id)
        except ResolveError as e:
            # One unavailable track must not sink the whole search: a
            # region-locked or deleted track is common in any long result
            # list, and the ones after it are usually fine.
            skipped.append(f"{title}: {e}")
            continue
        yield {
            "title": title,
            "artist": _artist_text(result),
            "album": str((result.get("album") or {}).get("name", "")).strip(),
            "duration": _duration(result),
            "video_id": video_id,
            "url": url,
            "artwork": _artwork_url(result, artwork_size),
            "source": _SOURCE,
        }
        yielded += 1

    if yielded == 0:
        detail = "; ".join(skipped[:3])
        raise ResolveError(
            f"nothing playable for {query!r}" + (f" ({detail})" if detail else "")
        )
    if skipped:
        print(
            f"skipped {len(skipped)} unplayable result(s): {skipped[0][:160]}",
            file=sys.stderr,
        )


def _cmd_search(args: argparse.Namespace) -> int:
    for row in search(args.query, args.count, args.artwork_size):
        print(json.dumps(row))
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="arc-music", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_search = sub.add_parser("search", help="resolve a query to playable tracks")
    p_search.add_argument("query", help="what to search YouTube Music for")
    p_search.add_argument("--count", type=int, default=1, help="how many results (default 1)")
    p_search.add_argument(
        "--artwork-size",
        type=int,
        default=544,
        help="largest cover-art width in pixels to hand back (default 544)",
    )
    p_search.set_defaults(func=_cmd_search)

    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except ResolveError as e:
        print(str(e), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())