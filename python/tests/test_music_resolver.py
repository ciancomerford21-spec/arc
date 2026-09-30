"""Tests for the YouTube Music resolver.

No network in any of these. `_stream_url` is the only thing that reaches
out, and it is replaced with a stub -- a test suite that searches YouTube
because it forgot to is a suite that fails on the user's afternoon.

One test does run against the real network, marked `live`, and is skipped
unless ARC_LIVE_MUSIC=1. It is the only way to find out the API's response
shape has moved.
"""

from __future__ import annotations

import json
import os

import pytest

from arc_music import (
    ResolveError,
    _artwork_url,
    _artist_text,
    _duration,
    _stream_url,
    search,
)


@pytest.fixture
def no_streams(monkeypatch):
    """Replace stream resolution with a recording stub."""
    calls: list[str] = []

    def fake(video_id: str) -> str:
        calls.append(video_id)
        return f"https://stream.invalid/{video_id}"

    monkeypatch.setattr("arc_music._stream_url", fake)
    return calls


@pytest.fixture
def fake_client(monkeypatch):
    """Replace the YouTube Music client with a canned response."""
    class Client:
        def __init__(self):
            self.queries: list[tuple[str, str, int]] = []

        def search(self, query, filter=None, limit=None):
            self.queries.append((query, filter, limit))
            return [
                {
                    "title": "Windowlicker",
                    "artists": [{"name": "Aphex Twin", "id": "UC1"}],
                    "album": {"name": "Windowlicker", "id": "MPRE1"},
                    "duration_seconds": 366,
                    "videoId": "abc123",
                    "thumbnails": [
                        {"url": "https://img.invalid/small", "width": 60},
                        {"url": "https://img.invalid/large", "width": 544},
                        {"url": "https://img.invalid/huge", "width": 1280},
                    ],
                }
            ]

    client = Client()
    monkeypatch.setattr("arc_music._client", lambda: client)
    return client


# --------------------------------------------------------------------------
# Field extraction
# --------------------------------------------------------------------------


def test_artist_list_is_flattened_to_a_comma_joined_line():
    # The API returns a list of objects; the UI wants one string, and a
    # feature keeps both names rather than picking one.
    assert _artist_text({"artists": [{"name": "Daft Punk"}, {"name": "Pharrell"}]}) == (
        "Daft Punk, Pharrell"
    )


def test_artist_handles_the_shapes_the_api_actually_returns():
    assert _artist_text({}) == ""
    assert _artist_text({"artists": None}) == ""
    assert _artist_text({"artists": []}) == ""
    # A malformed entry must not take the whole track down with it.
    assert _artist_text({"artists": [{"name": "Real"}, "junk", {"name": ""}]}) == "Real"


def test_artwork_picks_the_largest_that_still_fits():
    # Requesting a 48px list row should not download the 1280px original.
    thumbs = [
        {"url": "https://img/60", "width": 60},
        {"url": "https://img/544", "width": 544},
        {"url": "https://img/1280", "width": 1280},
    ]
    assert _artwork_url({"thumbnails": thumbs}, 48) == "https://img/60"
    assert _artwork_url({"thumbnails": thumbs}, 544) == "https://img/544"


def test_artwork_falls_back_when_every_size_is_too_big():
    thumbs = [{"url": "https://img/1280", "width": 1280}]
    assert _artwork_url({"thumbnails": thumbs}, 48) == "https://img/1280"


def test_a_playlist_hit_has_no_artwork_and_that_is_not_an_error():
    assert _artwork_url({"thumbnails": []}, 544) == ""
    assert _artwork_url({}, 544) == ""


def test_duration_ignores_what_the_api_omits():
    # Songs and live items come back with no duration_seconds, and 0 is the
    # value that means "the player will learn this later".
    assert _duration({"duration_seconds": 244}) == 244
    assert _duration({}) == 0
    assert _duration({"duration_seconds": None}) == 0
    assert _duration({"duration_seconds": -5}) == 0


# --------------------------------------------------------------------------
# search()
# --------------------------------------------------------------------------


def test_search_asks_for_songs_and_returns_a_playable_row(fake_client, no_streams):
    rows = list(search("aphex twin windowlicker", 1))

    assert len(rows) == 1
    row = rows[0]
    assert row["title"] == "Windowlicker"
    assert row["artist"] == "Aphex Twin"
    assert row["album"] == "Windowlicker"
    assert row["duration"] == 366
    assert row["video_id"] == "abc123"
    assert row["url"] == "https://stream.invalid/abc123"
    assert row["artwork"] == "https://img.invalid/large"
    assert row["source"] == "youtube music"
    # Every line must be JSON-serialisable: the contract with Rust is a
    # parsed JSON object per line, not a repr.
    assert json.loads(json.dumps(row)) == row


def test_search_filters_to_songs(fake_client, no_streams):
    # The unfiltered search mixes in videos, shorts and live covers, which
    # is how "play X" used to come back with a 40-minute upload.
    list(search("teardrop", 1))
    assert fake_client.queries[0][1] == "songs"


def test_resolved_urls_are_streamed_in_order(fake_client, no_streams):
    # Each id is resolved on its own, in result order, so the daemon's third
    # queued track is the third search result rather than whatever order a
    # batch call happened to answer in.
    list(search("x", 3))
    assert no_streams == ["abc123"]


def test_an_empty_query_is_refused_before_any_request(monkeypatch, fake_client):
    with pytest.raises(ResolveError, match="nothing to search for"):
        search("   ", 1)
    assert fake_client.queries == []


def test_results_without_a_title_or_id_are_skipped(monkeypatch, no_streams):
    class Client:
        def search(self, query, filter=None, limit=None):
            return [
                {"title": "", "videoId": "a"},
                {"title": "No id"},
                {"videoId": "c"},
                "not even a dict",
                {"title": "Real", "videoId": "d"},
            ]

    monkeypatch.setattr("arc_music._client", lambda: Client())
    rows = list(search("q", 5))
    assert [r["title"] for r in rows] == ["Real"]
    assert no_streams == ["d"]


def test_count_caps_how_many_are_returned(monkeypatch, no_streams):
    class Client:
        def search(self, query, filter=None, limit=None):
            return [{"title": f"t{i}", "videoId": str(i)} for i in range(10)]

    monkeypatch.setattr("arc_music._client", lambda: Client())
    assert len(list(search("q", 3))) == 3


def test_an_unplayable_track_does_not_sink_the_search(monkeypatch):
    # Region-locked and deleted tracks show up in any long result list. One
    # of them failing must still produce the ones after it.
    def fake_stream(video_id: str) -> str:
        if video_id == "bad":
            raise ResolveError("yt-dlp could not resolve bad: unavailable")
        return f"https://stream.invalid/{video_id}"

    monkeypatch.setattr("arc_music._stream_url", fake_stream)

    class Client:
        def search(self, query, filter=None, limit=None):
            return [
                {"title": "Bad", "videoId": "bad"},
                {"title": "Good", "videoId": "good"},
            ]

    monkeypatch.setattr("arc_music._client", lambda: Client())
    rows = list(search("q", 5))
    assert [r["title"] for r in rows] == ["Good"]


def test_when_every_result_is_unplayable_the_error_says_why(monkeypatch):
    def fake_stream(video_id: str) -> str:
        raise ResolveError("region locked")

    monkeypatch.setattr("arc_music._stream_url", fake_stream)

    class Client:
        def search(self, query, filter=None, limit=None):
            return [{"title": "Only One", "videoId": "x"}]

    monkeypatch.setattr("arc_music._client", lambda: Client())
    # "nothing playable" alone would be indistinguishable from a typo in the
    # query; the reason has to be in the message the user sees. The verdict
    # needs the results walked, so the iterator is consumed here.
    with pytest.raises(ResolveError, match="region locked"):
        list(search("q", 1))


def test_no_results_at_all_is_an_error_not_an_empty_list(monkeypatch):
    class Client:
        def search(self, query, filter=None, limit=None):
            return []

    monkeypatch.setattr("arc_music._client", lambda: Client())
    # An empty stdout is indistinguishable from "found nothing" to the Rust
    # side, so this has to fail loudly instead. Only walking the results can
    # reveal that, so the iterator is consumed.
    with pytest.raises(ResolveError, match="nothing playable"):
        list(search("nothing at all", 1))


# --------------------------------------------------------------------------
# _stream_url() against a yt-dlp stub
# --------------------------------------------------------------------------


def _stub_dlp(tmp_path, script: str):
    """A yt-dlp that records its argv and prints a fixed answer.

    Stricter than the real binary on purpose (see the skill's stub rule): the
    real yt-dlp exits nonzero on an unknown option, so a stub that accepted
    anything would let a wrong command line pass here and fail live.
    """
    path = tmp_path / "yt-dlp"
    path.write_text("#!/usr/bin/env python3\n" + script)
    path.chmod(0o755)
    return path


def test_stream_url_asks_yt_dlp_for_audio_only(tmp_path, monkeypatch):
    stub = _stub_dlp(
        tmp_path,
        "import sys\n"
        "assert '--get-url' in sys.argv, 'missing --get-url'\n"
        "assert '--no-playlist' in sys.argv, 'a video id must not pull a playlist'\n"
        "assert sys.argv[sys.argv.index('-f') + 1].startswith('bestaudio'), 'not audio-only'\n"
        "print('https://cdn.invalid/stream.webm')\n",
    )
    monkeypatch.setattr("arc_music._yt_dlp_binary", lambda: str(stub))
    assert _stream_url("abc") == "https://cdn.invalid/stream.webm"


def test_stream_url_reports_a_yt_dlp_failure(tmp_path, monkeypatch):
    stub = _stub_dlp(
        tmp_path,
        "import sys\n"
        "print('ERROR: video unavailable', file=sys.stderr)\n"
        "sys.exit(1)\n",
    )
    monkeypatch.setattr("arc_music._yt_dlp_binary", lambda: str(stub))
    with pytest.raises(ResolveError, match="unavailable"):
        _stream_url("abc")


def test_stream_url_rejects_empty_output(tmp_path, monkeypatch):
    # yt-dlp exiting 0 with nothing on stdout is a real failure mode; taking
    # that as a url would queue a track that cannot play.
    stub = _stub_dlp(tmp_path, "print('')\n")
    monkeypatch.setattr("arc_music._yt_dlp_binary", lambda: str(stub))
    with pytest.raises(ResolveError, match="no stream"):
        _stream_url("abc")


def test_a_missing_yt_dlp_is_named_in_the_error(monkeypatch):
    monkeypatch.setattr("arc_music.shutil.which", lambda _n: None)
    with pytest.raises(ResolveError, match="yt-dlp is not on PATH"):
        _stream_url("abc")


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def test_cli_prints_one_json_object_per_line(monkeypatch, capsys):
    from arc_music import main

    class Client:
        def search(self, query, filter=None, limit=None):
            return [
                {"title": "A", "videoId": "a", "artists": [{"name": "X"}]},
                {"title": "B", "videoId": "b", "artists": []},
            ]

    monkeypatch.setattr("arc_music._client", lambda: Client())
    monkeypatch.setattr(
        "arc_music._stream_url", lambda v: f"https://stream.invalid/{v}"
    )
    assert main(["search", "q", "--count", "2"]) == 0

    lines = capsys.readouterr().out.strip().splitlines()
    assert [json.loads(line)["title"] for line in lines] == ["A", "B"]


def test_cli_failure_exits_nonzero_with_the_reason_on_stderr(monkeypatch, capsys):
    from arc_music import main

    monkeypatch.setattr(
        "arc_music._client",
        lambda: (_ for _ in ()).throw(ResolveError("no session")),
    )
    assert main(["search", "q"]) == 1
    assert "no session" in capsys.readouterr().err


# --------------------------------------------------------------------------
# Live, opt-in
# --------------------------------------------------------------------------


@pytest.mark.live
def test_live_search_returns_a_playable_result():
    """The real API, the real yt-dlp. Skipped unless ARC_LIVE_MUSIC=1."""
    if not os.environ.get("ARC_LIVE_MUSIC"):
        pytest.skip("set ARC_LIVE_MUSIC=1 to hit the network")
    rows = list(search("aphex twin windowlicker", 1))
    assert rows[0]["title"]
    assert rows[0]["url"].startswith("http")
    assert rows[0]["artwork"].startswith("http")