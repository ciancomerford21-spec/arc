"""Model wrappers around sherpa-onnx: Silero VAD, Moonshine STT, Piper TTS.

Each wrapper loads lazily and raises :class:`EngineUnavailable` with a
user-facing reason when its model is missing, so the service can report
per-component health instead of crashing.
"""

from __future__ import annotations

import os
import re
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
    cpu = os.cpu_count() or 2
    # Avoid oversubscribing when the TTS backend wins the thread budget: cap
    # TTS work to a moderate number so the CPU stays responsive for audio I/O.
    return max(1, min(4, cpu // 2))


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


#: Kokoro v0.19 voice names -> speaker ids (af/am = American, bf/bm = British).
KOKORO_V019_SPEAKERS = {
    "af": 0, "af_bella": 1, "af_nicole": 2, "af_sarah": 3, "af_sky": 4, "am_adam": 5,
    "am_michael": 6, "bf_emma": 7, "bf_isabella": 8, "bm_george": 9, "bm_lewis": 10,
}


def kokoro_speaker_id(speaker: str) -> int:
    s = speaker.strip().lower()
    if not s:
        return 0
    if s.isdigit():
        return int(s)
    if s not in KOKORO_V019_SPEAKERS:
        raise EngineUnavailable(f"unknown kokoro voice {speaker!r}; one of: {', '.join(KOKORO_V019_SPEAKERS)}")
    return KOKORO_V019_SPEAKERS[s]


_SENTENCE_END = re.compile(r"(?<!%\.)(?<=[.!?…])\s+(?=[A-Z0-9\"'(])")


def split_sentences(text: str) -> list[str]:
    """Split a reply into sentences so the first can play while the rest render.

    Short fragments are merged into the previous sentence, so that a reply
    ending "...at 4%. Anything else?" plays as two clips rather than three:
    a one-clause aside does not benefit from its own round trip through the
    TTS queue. The test is word count, not characters -- "Anything else?" is
    13 characters but only two words, and a character count left it stranded
    as its own clip while the near-identical "4%." was merged.
    """
    parts = [p.strip() for p in _SENTENCE_END.split(text.strip()) if p.strip()]
    out: list[str] = []
    for p in parts:
        if out and len(p.split()) <= 3:
            out[-1] = f"{out[-1]} {p}"
        else:
            out.append(p)
    return out


class Tts:
    """Text to speech via sherpa-onnx.

    * ``piper``: fast VITS voices (one speaker per model).
    * ``kokoro``: much more natural, conversational prosody; several voices per
      model, picked with ``tts_speaker``. Slower (~0.5x real time here), so the
      service renders sentence by sentence and plays each as soon as it's ready.

    ``deterministic=True`` disables VITS sampling noise (tests only).
    """

    def __init__(self, cfg: VoiceConfig, deterministic: bool = False):
        if cfg.tts_engine == "none":
            raise EngineUnavailable("text-to-speech is disabled (voice.tts_engine = none)")
        if cfg.tts_engine not in ("piper", "kokoro"):
            raise EngineUnavailable(f"tts_engine {cfg.tts_engine!r} is not supported by the voice service yet")
        so = _import_sherpa()
        d = _need(resolve_model(cfg.tts_voice), "TTS voice")
        onnx = sorted(p for p in d.glob("*.onnx"))
        if not onnx:
            raise EngineUnavailable(f"no .onnx voice file in {d}")
        self.sid = 0
        if cfg.tts_engine == "kokoro":
            # Prefer the float model: on CPUs without VNNI the int8 build is ~4x slower.
            model = next((p for p in onnx if ".int8." not in p.name), onnx[0])
            self.sid = kokoro_speaker_id(cfg.tts_speaker)
            model_cfg = so.OfflineTtsModelConfig(
                kokoro=so.OfflineTtsKokoroModelConfig(
                    model=str(model),
                    voices=str(_need(d / "voices.bin", "Kokoro voices")),
                    tokens=str(_need(d / "tokens.txt", "TTS tokens")),
                    data_dir=str(_need(d / "espeak-ng-data", "espeak-ng data")),
                ),
                num_threads=max(1, min(6, (os.cpu_count() or 2) // 2)),
            )
        else:
            model_cfg = so.OfflineTtsModelConfig(
                vits=so.OfflineTtsVitsModelConfig(
                    model=str(onnx[0]),
                    tokens=str(_need(d / "tokens.txt", "TTS tokens")),
                    data_dir=str(_need(d / "espeak-ng-data", "espeak-ng data")),
                    **({"noise_scale": 0.0, "noise_scale_w": 0.0} if deterministic else {}),
                ),
                num_threads=_threads(),
            )
        self._tts = so.OfflineTts(so.OfflineTtsConfig(model=model_cfg, max_num_sentences=1))
        self.engine = cfg.tts_engine
        self.rate = cfg.speech_rate
        self.volume = cfg.volume

    def synthesize(self, text: str) -> Speech:
        g = self._tts.generate(text, sid=self.sid, speed=self.rate)
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
