"""Tests for playback serialisation.

Regression: Arc spoke over itself because ``Player.play`` opened a fresh
``pw-cat`` stream per clip and returned as soon as the samples were handed off,
so the next sentence started while the previous one was still audible.

These use a fake playback process, so they need no audio hardware.
"""

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


def test_empty_clip_is_a_no_op():
    p = Player("default")
    try:
        assert p.play(np.array([], dtype=np.float32), SR) is False
    finally:
        p.close()
