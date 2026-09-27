"""Tests for playback serialisation.

Regression: Arc spoke over itself because ``Player.play`` opened a fresh
``pw-cat`` stream per clip and returned as soon as the samples were handed off,
so the next sentence started while the previous one was still audible.

These use a fake playback process, so they need no audio hardware.
"""

import shutil
import subprocess
import sys
import threading
import time

import numpy as np
import pytest

from arc_voice.audio import AudioError, Player

pytestmark = pytest.mark.skipif(
    subprocess.run(["sh", "-c", "command -v pw-cat"], capture_output=True).returncode != 0,
    reason="pw-cat not installed",
)

SR = 22050


def _tone(seconds: float, amp: float = 1e-4) -> np.ndarray:
    return np.full(int(SR * seconds), amp, dtype=np.float32)


def test_play_blocks_for_the_clip_duration():
    """The core regression: play() must not return before audio finished."""
    p = Player("default")
    try:
        t0 = time.monotonic()
        p.play(_tone(0.5), SR)
        elapsed = time.monotonic() - t0
        assert elapsed >= 0.45, f"play() returned after {elapsed:.3f}s for a 0.5s clip"
    finally:
        p.close()


def test_sentences_are_close_but_not_overlapping():
    """Each sentence must start after the previous one finishes, and the gap
    must be small — a large gap reads as a stutter, an early start as overlap.
    """
    p = Player("default")
    clips = [_tone(0.5), _tone(0.7), _tone(0.4)]
    try:
        starts = []
        for c in clips:
            starts.append(time.monotonic())
            p.play(c, SR)
        for i in range(1, len(clips)):
            gap = starts[i] - starts[i - 1]
            prev_len = clips[i - 1].size / SR
            assert gap >= prev_len - 0.15, (
                f"clip {i} started {gap:.2f}s after a {prev_len:.2f}s clip — overlap"
            )
            assert gap <= prev_len + 0.45, (
                f"{gap - prev_len:.2f}s of dead air after clip {i} — sounds like a stutter"
            )
    finally:
        p.close()


def test_sequential_clips_do_not_overlap():
    """Three sentences in a row take at least as long as their audio."""
    p = Player("default")
    try:
        clips = [_tone(0.4) for _ in range(3)]
        t0 = time.monotonic()
        for c in clips:
            p.play(c, SR)
        elapsed = time.monotonic() - t0
        assert elapsed >= 1.15, f"1.2s of audio took {elapsed:.2f}s; clips overlapped"
    finally:
        p.close()


def test_playback_stream_uses_a_small_latency():
    """Regression: with pw-cat's default buffer the next sentence is queued
    while the current one drains, and the two overlap.
    """
    import arc_voice.audio as audio

    seen: list[list[str]] = []
    real_popen = audio.subprocess.Popen

    def fake_popen(cmd, *a, **kw):
        seen.append(list(cmd))
        raise RuntimeError("stop here")  # don't actually play

    p = Player("default")
    try:
        audio.subprocess.Popen = fake_popen
        try:
            p.play(_tone(0.1), SR)
        except Exception:
            pass
    finally:
        audio.subprocess.Popen = real_popen
        p.close()
    assert seen, "no playback process was started"
    cmd = seen[0]
    assert "--latency" in cmd, f"playback stream has no --latency: {cmd}"
    ms = int(cmd[cmd.index("--latency") + 1].rstrip("ms"))
    assert ms <= 100, f"latency {ms}ms is large enough to overlap sentences"


def test_concurrent_play_is_serialised():
    """The listening chime runs on its own thread; it must not talk over speech."""
    p = Player("default")
    try:
        long_clip, chime = _tone(0.8), _tone(0.2)
        t0 = time.monotonic()
        a = threading.Thread(target=lambda: p.play(long_clip, SR))
        a.start()
        time.sleep(0.05)
        b = threading.Thread(target=lambda: p.play(chime, SR))
        b.start()
        a.join(timeout=10)
        b.join(timeout=10)
        elapsed = time.monotonic() - t0
        assert elapsed >= 0.95, f"1.0s of audio took {elapsed:.2f}s; threads overlapped"
    finally:
        p.close()


def test_stop_interrupts_a_long_clip_promptly():
    """stop() must cut playback off, not wait for the whole clip."""
    p = Player("default")
    result: dict = {}
    try:
        clip = _tone(5.0)

        def run():
            t0 = time.monotonic()
            result["interrupted"] = p.play(clip, SR)
            result["elapsed"] = time.monotonic() - t0

        th = threading.Thread(target=run)
        th.start()
        time.sleep(0.6)
        p.stop()
        th.join(timeout=6)
        assert not th.is_alive(), "play() did not return after stop()"
        assert result.get("interrupted") is True
        assert result["elapsed"] < 3.0, f"stop() took {result['elapsed']:.2f}s to take effect"
    finally:
        p.close()


def test_stop_does_not_destroy_the_output_device():
    """Regression: stop() used `pw-cli destroy device:<name>`, which takes the
    whole output device offline and can silence unrelated players."""
    import arc_voice.audio as audio

    calls: list[list[str]] = []
    real_run = subprocess.run

    def fake_run(cmd, *a, **kw):
        calls.append(list(cmd) if isinstance(cmd, list) else [str(cmd)])
        return real_run(["true"], *a, **kw)

    p = Player("default")
    try:
        audio.subprocess.run = fake_run
        p.stop()
    finally:
        audio.subprocess.run = real_run
        p.close()
    assert not any("destroy" in c for c in calls), f"stop() tore down a device: {calls}"


@pytest.mark.skipif(
    not shutil.which("pw-cat"), reason="pw-cat not installed"
)
def test_deployed_copy_matches_the_repo() -> None:
    """The service runs an *installed copy* of arc_voice, not the repo.

    A stale copy means fixes are live-tested against old code, which is how
    the sentence-overlap bug survived two rounds of "fixes". Keep them
    byte-identical.
    """
    from pathlib import Path

    repo = Path(__file__).resolve().parents[1] / "arc_voice"
    deployed = Path.home() / ".local/share/arc/python/arc_voice"
    if not deployed.exists():
        pytest.skip("arc_voice is not installed (nothing to compare)")
    assert shutil.which("pw-cat"), "no PipeWire session"

    differing = []
    for src in sorted(repo.rglob("*.py")):
        if "__pycache__" in src.parts:
            continue
        rel = src.relative_to(repo)
        dst = deployed / rel
        if not dst.exists():
            differing.append(f"{rel} (missing)")
        elif dst.read_bytes() != src.read_bytes():
            differing.append(f"{rel} (differs)")
    assert not differing, (
        "the installed arc_voice is stale — run scripts/install.sh or: "
        f"rm -rf {deployed} && cp -r {repo} {deployed}\nstale: {differing}"
    )

@pytest.mark.skipif(
    not shutil.which("pw-cat"), reason="pw-cat not installed"
)
def test_deployed_copy_has_the_overlap_fixes() -> None:
    """Spot-check the two things that stop sentences overlapping."""
    from pathlib import Path

    audio = Path.home() / ".local/share/arc/python/arc_voice/audio.py"
    if not audio.exists():
        pytest.skip("arc_voice is not installed")
    src = audio.read_text()
    assert '--latency", "40ms"' in src, "playback stream has no small --latency"
    assert "_play_locked" in src, "play() is not serialised"
    assert "destroy" not in src.split("def stop")[1].split("def ")[0], (
        "stop() must not destroy the output device"
    )

def test_empty_clip_is_a_no_op():
    p = Player("default")
    try:
        assert p.play(np.array([], dtype=np.float32), SR) is False
    finally:
        p.close()
