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


def _read_wav(path: str) -> tuple[np.ndarray, int]:
    with wave.open(path, "rb") as w:
        if w.getsampwidth() != 2:
            raise SystemExit(f"{path}: only 16-bit PCM WAV is supported")
        a = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float32) / 32768.0
        if w.getnchannels() > 1:
            a = a.reshape(-1, w.getnchannels()).mean(axis=1)
        return a, w.getframerate()


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

    from .config import VoiceConfig

    cfg = VoiceConfig.load(Path(args.config) if args.config else None)

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
