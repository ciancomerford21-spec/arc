"""Voice settings, read from the same ``config.toml`` as the daemon.

Only the ``[voice]`` and ``[general]`` sections are used. Every field has the
same default as ``arc_config::Voice`` so a missing file or key is fine.
"""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field, fields
from pathlib import Path


def _xdg(var: str, fallback: str) -> Path:
    v = os.environ.get(var)
    return Path(v) if v else Path.home() / fallback


def config_file() -> Path:
    d = os.environ.get("ARC_CONFIG_DIR")
    return (Path(d) if d else _xdg("XDG_CONFIG_HOME", ".config") / "arc") / "config.toml"


def data_dir() -> Path:
    d = os.environ.get("ARC_DATA_DIR")
    return Path(d) if d else _xdg("XDG_DATA_HOME", ".local/share") / "arc"


def models_dir() -> Path:
    return data_dir() / "models"


def resolve_model(name: str) -> Path:
    """Model name relative to the models dir, or an absolute / ~ path."""
    p = Path(os.path.expanduser(name))
    return p if p.is_absolute() else models_dir() / p


@dataclass
class VoiceConfig:
    enabled: bool = True
    mode: str = "wake_word"  # push_to_talk | wake_word | continuous
    wake_words: list[str] = field(default_factory=lambda: ["hey arc", "okay arc", "ok arc"])
    wake_engine: str = "stt"  # stt | kws
    input_device: str = "default"
    output_device: str = "default"
    speech_rate: float = 1.0
    volume: float = 0.8
    speak_replies: bool = True
    speak_text_replies: bool = False
    chime: bool = True
    max_utterance_s: float = 15.0
    end_silence_ms: int = 700
    no_speech_timeout_s: float = 6.0
    vad_threshold: float = 0.5
    kws_threshold: float = 0.25
    stt_engine: str = "moonshine"  # moonshine | whisper | voxtype
    stt_model: str = "sherpa-onnx-moonshine-base-en-quantized-2026-02-27"
    tts_engine: str = "piper"  # piper | kokoro | espeak | none
    tts_voice: str = "vits-piper-en_GB-jenny_dioco-medium"
    tts_speaker: str = ""  # kokoro voice name ("bf_emma") or id; "" = first
    barge_in: bool = True
    preload: bool = True
    follow_up: bool = True
    voice_confirm_dangerous: bool = False
    # From [general]:
    name_variants: list[str] = field(default_factory=lambda: ["arc", "ark"])

    @classmethod
    def from_toml(cls, data: dict) -> "VoiceConfig":
        known = {f.name for f in fields(cls)}
        voice = {k: v for k, v in (data.get("voice") or {}).items() if k in known}
        general = data.get("general") or {}
        cfg = cls(**voice)
        if isinstance(general.get("name_variants"), list):
            cfg.name_variants = [str(x) for x in general["name_variants"]]
        cfg.validate()
        return cfg

    @classmethod
    def load(cls, path=None):
        p = path
        if isinstance(p, str):
            p = Path(p)
        p = p or cls.config_file()
        if not p.exists():
            return cls()
        with p.open("rb") as f:
            return cls.from_toml(tomllib.load(f))

    def validate(self) -> None:
        if self.mode not in ("push_to_talk", "wake_word", "continuous"):
            raise ValueError(f"voice.mode: unknown mode {self.mode!r}")
        self.speech_rate = min(max(float(self.speech_rate), 0.5), 2.0)
        self.volume = min(max(float(self.volume), 0.0), 1.0)
        self.vad_threshold = min(max(float(self.vad_threshold), 0.05), 0.95)
        self.end_silence_ms = max(int(self.end_silence_ms), 150)
        self.max_utterance_s = max(float(self.max_utterance_s), 2.0)
        self.no_speech_timeout_s = max(float(self.no_speech_timeout_s), 1.0)
        self.wake_words = [w.strip().lower() for w in self.wake_words if w.strip()]
        self.name_variants = [n.strip().lower() for n in self.name_variants if n.strip()]
