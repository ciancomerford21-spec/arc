"""Real PipeWire playback/record through the same code the service uses.

Skipped when no PipeWire session is reachable (CI). Plays ~0.3 s of near-silence
so it's inaudible, and asserts the player process ran for the clip's duration
instead of exiting at once (the pw-play "Format not recognised" regression).
"""

import shutil
import subprocess
import time

import numpy as np
import pytest

from arc_voice.audio import Player


def _pipewire_up() -> bool:
    if not shutil.which("pw-cat"):
        return False
    return subprocess.run(["pw-cli", "info", "0"], capture_output=True, timeout=3).returncode == 0


pytestmark = pytest.mark.skipif(not _pipewire_up(), reason="no PipeWire session")


def test_player_actually_plays_raw_pcm():
    sr = 22050
    clip = np.full(int(sr * 0.3), 1e-4, dtype=np.float32)
    t = time.monotonic()
    interrupted = Player("default").play(clip, sr)
    took = time.monotonic() - t
    assert interrupted is False
    assert took >= 0.25, f"player returned after {took:.3f}s; audio was not played"
