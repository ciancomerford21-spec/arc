"""Entry point: ``python -m arc_voice``.

Normally started by the Arc daemon. Standalone use (for debugging)::

    python -m arc_voice --no-audio          # protocol only, no mic/speaker
    python -m arc_voice --transcribe a.wav  # run STT on a file and exit
    python -m arc_voice --say "hello"       # speak once and exit
"""
from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import wave

import numpy as np

from .config import VoiceConfig


DEFAULT_OUTPUT = "auto"
DEFAULT_INPUT = "auto"


def _default_output_device() -> str:
    """Best-effort default output device (won't raise on PipeWire absences)."""
    try:
        probe = __import__("arc_voice.audio", fromlist=["_probe"])
    except Exception:
        return DEFAULT_OUTPUT
    try:
        p = probe._probe()
        if p is None:
            return DEFAULT_OUTPUT
        # Prefer a non-monitor sink if one exists.
        for name in p.get("sinks", []) or []:
            n = name.get("name") or ""
            if "none" not in n.lower() and "monitor" not in n.lower():
                return n
        return (p.get("default_sink") or [DEFAULT_OUTPUT])[0]
    except Exception:
        return DEFAULT_OUTPUT


def _default_input_device() -> str:
    """Best-effort default input device (won't raise on PipeWire absences)."""
    try:
        probe = __import__("arc_voice.audio", fromlist=["_probe"])
    except Exception:
        return DEFAULT_INPUT
    try:
        p = probe._probe()
        if p is None:
            return DEFAULT_INPUT
        return (p.get("default_source") or [DEFAULT_INPUT])[0]
    except Exception:
        return DEFAULT_INPUT


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="arc-voice")
    ap.add_argument("--config", help="path to config.toml")
    ap.add_argument("--no-audio", action="store_true", help="don't open microphone or speaker")
    ap.add_argument("--transcribe", metavar="WAV", help="transcribe a WAV file and exit")
    ap.add_argument("--say", metavar="TEXT", help="speak text and exit")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args(argv)

    logging.basicConfig(
        stream=sys.stderr,
        level=logging.DEBUG if args.verbose or os.environ.get("ARC_VOICE_DEBUG") else logging.INFO,
        format="arc-voice %(levelname)s %(message)s",
    )

    from pathlib import Path

    cfg = VoiceConfig.load(Path(args.config) if args.config else None)

    # CLI flags override config values when --no-audio is NOT used, so a
    # daemon-started service can be run standalone with the same defaults the
    # daemon would pass.
    if not args.no_audio:
        cfg.output_device = os.environ.get("ARC_OUTPUT_DEVICE", cfg.output_device or _default_output_device())
        cfg.input_device = os.environ.get("ARC_INPUT_DEVICE", cfg.input_device or _default_input_device())

    if args.transcribe:
        from .engines import Stt

        audio, sr = _read_wav(args.transcribe)
        tr = Stt(cfg).transcribe(audio, sr)
        print(json.dumps({"text": tr.text, "stt_ms": tr.stt_ms, "audio_ms": tr.audio_ms}))
        return 0

    if args.say:
        from .audio import Player
        from .engines import Tts

        sp = Tts(cfg).synthesize(args.say)
        Player(cfg.output_device).play(sp.samples, sp.sample_rate)
        return 0

    from .service import Service

    # Line-buffered stdout even when it's a pipe.
    sys.stdout.reconfigure(line_buffering=True)
    return Service(cfg, use_audio=not args.no_audio).run()


if __name__ == "__main__":
    sys.exit(main())
