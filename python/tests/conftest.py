"""Shared fixtures.

Model-backed tests use the real models in ``$ARC_DATA_DIR/models`` (default
``~/.local/share/arc/models``) and are skipped when they are missing.
Utterances are synthesised with the Piper voice and resampled to 16 kHz,
which makes the STT/VAD tests deterministic without shipping audio files.
"""

from __future__ import annotations

import numpy as np
import pytest

from arc_voice.config import VoiceConfig
from arc_voice.engines import FRAME, SAMPLE_RATE, EngineUnavailable


def _resample(x: np.ndarray, sr: int) -> np.ndarray:
    if sr == SAMPLE_RATE:
        return x.astype(np.float32)
    n = int(len(x) * SAMPLE_RATE / sr)
    return np.interp(np.linspace(0, len(x) - 1, n), np.arange(len(x)), x).astype(np.float32)


@pytest.fixture(scope="session")
def cfg() -> VoiceConfig:
    c = VoiceConfig()
    c.volume = 1.0
    return c


@pytest.fixture(scope="session")
def vad_factory(cfg):
    from arc_voice.engines import Vad

    try:
        Vad(cfg)
    except EngineUnavailable as e:
        pytest.skip(str(e))
    return lambda: Vad(cfg)


@pytest.fixture(scope="session")
def stt(cfg):
    from arc_voice.engines import Stt

    try:
        return Stt(cfg)
    except EngineUnavailable as e:
        pytest.skip(str(e))


@pytest.fixture(scope="session")
def tts(cfg):
    from arc_voice.engines import Tts

    try:
        # Deterministic: identical audio every run, so STT results are stable.
        return Tts(cfg, deterministic=True)
    except EngineUnavailable as e:
        pytest.skip(str(e))


@pytest.fixture(scope="session")
def say(tts):
    """Render text to 16 kHz float32 speech (cached)."""
    cache: dict[str, np.ndarray] = {}

    def render(text: str) -> np.ndarray:
        if text not in cache:
            sp = tts.synthesize(text)
            cache[text] = _resample(sp.samples, sp.sample_rate)
        return cache[text]

    return render


def silence(seconds: float) -> np.ndarray:
    return np.zeros(int(SAMPLE_RATE * seconds), dtype=np.float32)


def frames(audio: np.ndarray):
    pad = (-len(audio)) % FRAME
    audio = np.concatenate([audio, np.zeros(pad, dtype=np.float32)])
    for i in range(0, len(audio), FRAME):
        yield audio[i : i + FRAME]
