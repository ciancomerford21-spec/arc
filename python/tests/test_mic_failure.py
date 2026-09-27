"""Unit tests for Microphone failure reporting.

Regression: ``Microphone._read`` used to ``break`` silently when the capture
process (``pw-cat``) exited, so the voice service kept reporting "ready" while
hearing nothing. These tests replace the capture process with a fake, so they
need no PipeWire session and no audio hardware.
"""

import subprocess
import sys
import time

import numpy as np

from arc_voice.audio import Microphone

# Minimal stand-in for pw-cat: emits float32 silence to stdout until killed or
# until it exits on its own, exactly like the real one when a driver hiccups.
_FAKE_STDOUT = """
import sys, time
out = sys.stdout.buffer
frame = b"\\x00" * (320 * 4)
while True:
    out.write(frame)
    out.flush()
    time.sleep(0.02)
"""

_FAKE_EXITS = """
import sys, time
out = sys.stdout.buffer
frame = b"\\x00" * (320 * 4)
for _ in range(3):
    out.write(frame)
    out.flush()
    time.sleep(0.02)
sys.exit(1)
"""


def _mic(errors, frames, script=_FAKE_STDOUT):
    """A Microphone whose capture is a fake process, not a real device."""
    mic = Microphone("default", frames.append, errors.append)
    # Replace the process *before* start() spawns pw-cat, by pointing
    # Microphone at a fake executable via monkeypatching shutil.which.
    import arc_voice.audio as audio

    real_start = audio.Microphone.start

    def fake_start(self):
        self._proc = subprocess.Popen(
            [sys.executable, "-c", script], stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0
        )
        self._stopping = False
        self._thread = self._thread or __import__("threading").Thread(
            target=self._read, name="mic", daemon=True
        )
        self._thread.start()

    audio.Microphone.start = fake_start
    try:
        mic.start()
    finally:
        audio.Microphone.start = real_start
    return mic


def _wait_for(predicate, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return False


def test_capture_running_reports_alive():
    errors: list[str] = []
    mic = _mic(errors, [])
    try:
        assert _wait_for(mic.is_alive)
        assert mic.is_alive()
    finally:
        mic.stop()


def test_dying_capture_surfaces_an_error():
    """The point of the fix: a dead mic must be visible, not silent."""
    errors: list[str] = []
    mic = _mic(errors, [], script=_FAKE_EXITS)
    # The fake exits by itself, so the error should arrive on its own.
    assert _wait_for(lambda: bool(errors)), "mic failure was swallowed"
    assert "ended unexpectedly" in errors[0], errors[0]


def test_frames_flow_while_capture_is_alive():
    errors: list[str] = []
    frames: list[np.ndarray] = []
    mic = _mic(errors, frames)
    try:
        assert _wait_for(lambda: len(frames) >= 3), f"only got {len(frames)} frames"
        assert errors == []
    finally:
        mic.stop()


def test_deliberate_stop_is_not_reported_as_an_error():
    errors: list[str] = []
    mic = _mic(errors, [])
    assert _wait_for(mic.is_alive)
    mic.stop()
    time.sleep(0.3)
    assert errors == [], f"stop() should not look like a fault: {errors}"
