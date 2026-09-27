"""Model wrappers around sherpa-onnx: Silero VAD, Moonshine STT, Piper TTS.

Each wrapper loads lazily and raises :class:`EngineUnavailable` with a
user-facing reason when its model is missing, so the service can report
per-component health instead of crashing.
"""

from __future__ import annotations

import os
import time
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from .config import VoiceConfig, models_dir, resolve_model

SAMPLE_RATE = 16000
#: Silero VAD window at 16 kHz (32 ms).
FRAME = 512


class EngineUnavailable(RuntimeError):
    pass


def _threads() -> int:
    return max(1, min(4, (os.cpu_count() or 2) // 2))


def _need(p: Path, what: str) -> Path:
    if not p.exists():
        raise EngineUnavailable(f"{what} not found at {p} (run scripts/fetch-models.sh)")
    return p


def _import_sherpa():
    try:
        import sherpa_onnx  # noqa: PLC0415

        return sherpa_onnx
    except ImportError as e:  # pragma: no cover - depends on env
        raise EngineUnavailable(f"sherpa-onnx is not installed: {e}") from e


# ---------------------------------------------------------------------------
# VAD
# ---------------------------------------------------------------------------


class Vad:
    """Frame-level speech probability gate (Silero via sherpa-onnx).

    Only :meth:`is_speech` is used: segmentation (start/end of an utterance,
    silence timeouts) is done by :class:`arc_voice.pipeline.Pipeline` so its
    timing is explicit and testable.
    """

    def __init__(self, cfg: VoiceConfig):
        so = _import_sherpa()
        c = so.VadModelConfig()
        c.silero_vad.model = str(_need(models_dir() / "silero_vad.onnx", "Silero VAD model"))
        c.silero_vad.threshold = cfg.vad_threshold
        # Very short internal smoothing: we only read the per-frame flag.
        c.silero_vad.min_silence_duration = 0.1
        c.silero_vad.min_speech_duration = 0.1
        c.silero_vad.window_size = FRAME
        c.sample_rate = SAMPLE_RATE
        c.num_threads = 1
        self._vad = so.VoiceActivityDetector(c, buffer_size_in_seconds=2)

    def is_speech(self, frame: np.ndarray) -> bool:
        self._vad.accept_waveform(frame)
        speaking = self._vad.is_speech_detected()
        # Drain finished segments; we keep our own buffer.
        while not self._vad.empty():
            self._vad.pop()
        return speaking

    def reset(self) -> None:
        self._vad.reset()


# ---------------------------------------------------------------------------
# STT
# ---------------------------------------------------------------------------


@dataclass
class Transcript:
    text: str
    stt_ms: int
    audio_ms: int


class Stt:
    def __init__(self, cfg: VoiceConfig):
        if cfg.stt_engine != "moonshine":
            raise EngineUnavailable(f"stt_engine {cfg.stt_engine!r} is not supported by the voice service yet")
        so = _import_sherpa()
        d = _need(resolve_model(cfg.stt_model), "speech-to-text model")
        tokens = _need(d / "tokens.txt", "STT tokens")
        if (d / "encoder_model.ort").exists():
            # Moonshine v2 layout (2026 releases).
            self._rec = so.OfflineRecognizer.from_moonshine_v2(
                encoder=str(d / "encoder_model.ort"),
                decoder=str(_need(d / "decoder_model_merged.ort", "STT decoder")),
                tokens=str(tokens),
                num_threads=_threads(),
            )
        else:
            self._rec = so.OfflineRecognizer.from_moonshine(
                preprocessor=str(_need(d / "preprocess.onnx", "STT preprocessor")),
                encoder=str(_need(d / "encode.int8.onnx", "STT encoder")),
                uncached_decoder=str(_need(d / "uncached_decode.int8.onnx", "STT decoder")),
                cached_decoder=str(_need(d / "cached_decode.int8.onnx", "STT decoder")),
                tokens=str(tokens),
                num_threads=_threads(),
            )

    def transcribe(self, audio: np.ndarray, sample_rate: int = SAMPLE_RATE) -> Transcript:
        t = time.monotonic()
        s = self._rec.create_stream()
        s.accept_waveform(sample_rate, audio)
        self._rec.decode_stream(s)
        text = " ".join(s.result.text.split())
        return Transcript(text, int((time.monotonic() - t) * 1000), int(len(audio) * 1000 / sample_rate))


# ---------------------------------------------------------------------------
# TTS
# ---------------------------------------------------------------------------


@dataclass
class Speech:
    samples: np.ndarray  # float32 mono
    sample_rate: int


class Tts:
    """Piper TTS. ``deterministic=True`` disables VITS sampling noise so the
    same text always yields identical audio (used by tests; slightly flatter
    prosody, so the service keeps the model defaults)."""

    def __init__(self, cfg: VoiceConfig, deterministic: bool = False):
        if cfg.tts_engine == "none":
            raise EngineUnavailable("text-to-speech is disabled (voice.tts_engine = none)")
        if cfg.tts_engine != "piper":
            raise EngineUnavailable(f"tts_engine {cfg.tts_engine!r} is not supported by the voice service yet")
        so = _import_sherpa()
        d = _need(resolve_model(cfg.tts_voice), "TTS voice")
        onnx = sorted(p for p in d.glob("*.onnx"))
        if not onnx:
            raise EngineUnavailable(f"no .onnx voice file in {d}")
        c = so.OfflineTtsConfig(
            model=so.OfflineTtsModelConfig(
                vits=so.OfflineTtsVitsModelConfig(
                    model=str(onnx[0]),
                    tokens=str(_need(d / "tokens.txt", "TTS tokens")),
                    data_dir=str(_need(d / "espeak-ng-data", "espeak-ng data")),
                    **({"noise_scale": 0.0, "noise_scale_w": 0.0} if deterministic else {}),
                ),
                num_threads=_threads(),
            ),
            max_num_sentences=1,
        )
        self._tts = so.OfflineTts(c)
        self.rate = cfg.speech_rate
        self.volume = cfg.volume

    def synthesize(self, text: str) -> Speech:
        g = self._tts.generate(text, sid=0, speed=self.rate)
        samples = np.asarray(g.samples, dtype=np.float32) * self.volume
        return Speech(samples, g.sample_rate)


def chime(rising: bool, sample_rate: int = 22050, volume: float = 0.25) -> Speech:
    """Short two-tone chime: rising = start listening, falling = stop."""
    freqs = (660.0, 880.0) if rising else (880.0, 660.0)
    n = int(sample_rate * 0.07)
    t = np.arange(n) / sample_rate
    env = np.sin(np.pi * np.arange(n) / n)
    parts = [np.sin(2 * np.pi * f * t) * env for f in freqs]
    return Speech((np.concatenate(parts) * volume).astype(np.float32), sample_rate)
