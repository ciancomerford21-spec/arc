"""Voice pipeline state machine.

Pure logic: frames in, reports out. Engines (VAD/STT) are injected, and time
is measured in frames (32 ms each), so the whole pipeline is testable on WAV
files without a microphone or real clock.

States
------
``idle``      Not capturing. In wake-word / continuous mode the VAD still runs
              and speech segments are transcribed to look for the wake phrase.
``listening`` Capturing a command utterance (after PTT, wake word, or a
              follow-up). Ends on trailing silence, the utterance limit,
              ``stop_listening``, or no speech within the timeout.

Wake phrase + command in one breath ("hey arc, lock the screen") yields the
command directly, without a second capture.
"""

from __future__ import annotations

import logging
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass
from typing import Protocol

import numpy as np

from .engines import FRAME, SAMPLE_RATE, Transcript
from .wake import WakeMatcher

FRAME_MS = FRAME * 1000 // SAMPLE_RATE  # 32

log = logging.getLogger("arc_voice.pipeline")


class VadLike(Protocol):
    def is_speech(self, frame: np.ndarray) -> bool: ...
    def reset(self) -> None: ...


class SttLike(Protocol):
    def transcribe(self, audio: np.ndarray, sample_rate: int = SAMPLE_RATE) -> Transcript: ...


Report = dict
Emit = Callable[[Report], None]


@dataclass
class Timing:
    end_silence_ms: int = 700
    max_utterance_s: float = 15.0
    no_speech_timeout_s: float = 6.0
    #: Audio kept from before speech onset so the first syllable isn't cut.
    preroll_ms: int = 320
    #: Idle-mode segments shorter than this are ignored (clicks, coughs).
    min_segment_ms: int = 250
    #: Idle-mode segments are capped (nobody says a wake phrase for 8 s).
    max_wake_segment_s: float = 8.0

    def frames(self, ms: float) -> int:
        return max(1, int(ms // FRAME_MS))


class Pipeline:
    def __init__(self, vad: VadLike, stt: SttLike, wake: WakeMatcher, emit: Emit,
                 mode: str = "wake_word", timing: Timing | None = None):
        self.vad = vad
        self.stt = stt
        self.wake = wake
        self.emit = emit
        self.mode = mode
        self.t = timing or Timing()
        self.state = "idle"
        self._pre: deque[np.ndarray] = deque(maxlen=self.t.frames(self.t.preroll_ms))
        self._seg: list[np.ndarray] = []
        self._in_speech = False
        self._silence = 0
        self._frames = 0
        self._heard = False
        self._level_every = 3  # ~100 ms
        self._level_count = 0
        #: Per-capture override of no_speech_timeout_s (follow-ups).
        self._timeout_s: float | None = None
        #: Set by the service while TTS plays; idle segments are then only
        #: checked for the wake phrase (barge-in), never used as commands.
        self.speaking = False

    # -- control --------------------------------------------------------

    def set_mode(self, mode: str) -> None:
        self.mode = mode
        if self.state == "idle":
            self._reset_segment()

    def start_listening(self, timeout_s: float | None = None) -> None:
        """Begin a command capture. ``timeout_s`` overrides how long to wait for
        speech to start (follow-ups use a shorter window)."""
        if self.state == "listening":
            return
        self.state = "listening"
        self._seg = list(self._pre) if self._in_speech else []
        self._frames = 0
        self._silence = 0
        self._heard = self._in_speech
        self._timeout_s = timeout_s
        self.emit({"report": "listening_started"})

    def stop_listening(self) -> None:
        """End the capture now and transcribe what we have."""
        if self.state == "listening":
            self._finish_command()

    def cancel_listening(self) -> None:
        if self.state == "listening":
            self._to_idle()
            self.emit({"report": "no_speech"})

    def toggle_listening(self) -> None:
        if self.state == "listening":
            self.stop_listening()
        else:
            self.start_listening()

    # -- frames ---------------------------------------------------------

    def feed(self, frame: np.ndarray) -> None:
        speech = self.vad.is_speech(frame)
        if self.state == "listening":
            self._feed_listening(frame, speech)
        else:
            self._feed_idle(frame, speech)

    def _feed_listening(self, frame: np.ndarray, speech: bool) -> None:
        self._seg.append(frame)
        self._frames += 1
        self._level_count += 1
        if self._level_count >= self._level_every:
            self._level_count = 0
            rms = float(np.sqrt(np.mean(np.square(frame))))
            self.emit({"report": "level", "rms": round(min(1.0, rms * 4), 3)})
        if speech:
            self._heard = True
            self._silence = 0
        else:
            self._silence += 1
        self._in_speech = speech
        timeout_s = self._timeout_s if self._timeout_s is not None else self.t.no_speech_timeout_s
        if not self._heard and self._frames >= self.t.frames(timeout_s * 1000):
            self._to_idle()
            self.emit({"report": "no_speech"})
        elif self._heard and self._silence >= self.t.frames(self.t.end_silence_ms):
            self._finish_command()
        elif self._frames >= self.t.frames(self.t.max_utterance_s * 1000):
            self._finish_command()

    def _feed_idle(self, frame: np.ndarray, speech: bool) -> None:
        if self.mode == "push_to_talk" and not self.speaking:
            self._pre.append(frame)
            self._in_speech = speech
            return
        if not self._in_speech:
            if speech:
                self._in_speech = True
                self._seg = list(self._pre)
                self._seg.append(frame)
                self._silence = 0
            else:
                self._pre.append(frame)
            return
        self._seg.append(frame)
        if speech:
            self._silence = 0
        else:
            self._silence += 1
        too_long = len(self._seg) >= self.t.frames(self.t.max_wake_segment_s * 1000)
        if self._silence >= self.t.frames(self.t.end_silence_ms) or too_long:
            seg = self._seg
            self._reset_segment()
            self._idle_segment(seg)

    def _idle_segment(self, seg: list[np.ndarray]) -> None:
        voiced = len(seg) - self._silence
        if voiced * FRAME_MS < self.t.min_segment_ms:
            return
        tr = self.stt.transcribe(np.concatenate(seg))
        # Debug only: idle speech is often other people's conversation.
        log.debug("heard (idle, %d ms): %r", len(seg) * FRAME_MS, tr.text)
        if not tr.text:
            return
        m = self.wake.match(tr.text)
        if m is None:
            if self.mode == "continuous" and not self.speaking:
                self._emit_transcript(tr.text, tr)
            return
        self.emit({"report": "wake_detected", "keyword": m.phrase})
        if m.command and len(m.command.split()) >= 1 and not self._only_punct(m.command):
            self._emit_transcript(m.command, tr)
        else:
            self.start_listening()
            # The wake phrase itself is not part of the command.
            self._seg = []
            self._heard = False

    # -- helpers --------------------------------------------------------

    @staticmethod
    def _only_punct(s: str) -> bool:
        return not any(c.isalnum() for c in s)

    def _finish_command(self) -> None:
        seg = self._seg
        heard = self._heard
        self._to_idle()
        if not heard or not seg:
            self.emit({"report": "no_speech"})
            return
        self.emit({"report": "transcribing"})
        tr = self.stt.transcribe(np.concatenate(seg))
        text = tr.text
        # Saying the wake phrase again after the chime is common; drop it.
        m = self.wake.match(text)
        if m is not None:
            text = m.command
        if not text or self._only_punct(text):
            self.emit({"report": "no_speech"})
            return
        self._emit_transcript(text, tr)

    def _emit_transcript(self, text: str, tr: Transcript) -> None:
        self.emit({"report": "transcript", "text": text, "stt_ms": tr.stt_ms, "audio_ms": tr.audio_ms})

    def _reset_segment(self) -> None:
        self._seg = []
        self._in_speech = False
        self._silence = 0

    def _to_idle(self) -> None:
        self.state = "idle"
        self._reset_segment()
        self._pre.clear()
        self._frames = 0
        self._heard = False
        self.vad.reset()
