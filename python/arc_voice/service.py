"""The voice service process: JSON lines on stdin/stdout.

Threads
-------
* main      – reads commands from stdin.
* mic       – pw-record reader; puts frames on a queue.
* worker    – owns the pipeline (VAD + STT run here, never on the mic thread).
* speaker   – plays queued utterances; one at a time.

All stdout writes go through :meth:`Service.emit` (locked, one JSON per line).
"""

from __future__ import annotations

import json
import logging
import queue
import sys
import threading
import time
from typing import IO

import numpy as np

from . import __version__
from .audio import AudioError, Microphone, Player
from .config import VoiceConfig
from .engines import EngineUnavailable, Stt, Tts, Vad, chime, split_sentences
from .pipeline import Pipeline, Timing
from .wake import WakeMatcher

log = logging.getLogger("arc_voice")


def ok(detail: str = "") -> dict:
    return {"status": "ok", "detail": detail}


def unavailable(detail: str) -> dict:
    return {"status": "unavailable", "detail": detail}


def disabled() -> dict:
    return {"status": "disabled", "detail": ""}


#: Seconds to wait for an answer after Arc asks something, before giving up quietly.
FOLLOW_UP_TIMEOUT_S = 5.0


class Service:
    def __init__(self, cfg: VoiceConfig, out: IO[str] = sys.stdout, use_audio: bool = True):
        self.cfg = cfg
        self._out = out
        self._out_lock = threading.Lock()
        self._frames: queue.Queue[np.ndarray | None] = queue.Queue(maxsize=200)
        self._ctl: queue.Queue[tuple[str, dict]] = queue.Queue()
        self._speech: queue.Queue[tuple[str, str, bool] | None] = queue.Queue()
        self.use_audio = use_audio
        self.health: dict[str, dict] = {}
        self.vad = self.stt = self.tts = None
        self.pipeline: Pipeline | None = None
        self.mic: Microphone | None = None
        self.player = Player(cfg.output_device)
        self._stop = threading.Event()
        #: Set to abandon the reply currently being rendered/played.
        self._cancel_speech = threading.Event()

    # -- output ---------------------------------------------------------

    def emit(self, report: dict) -> None:
        line = json.dumps(report, separators=(",", ":"), ensure_ascii=False)
        with self._out_lock:
            self._out.write(line + "\n")
            self._out.flush()
        if report.get("report") not in ("level",):
            log.debug("-> %s", line)

    def _on_pipeline_report(self, r: dict) -> None:
        kind = r.get("report")
        if kind == "listening_started" and self.cfg.chime:
            self._chime(True)
        if kind == "wake_detected" and self.player.playing and self.cfg.barge_in:
            self._cancel_speech.set()
            self.player.stop()
        self.emit(r)

    def _chime(self, rising: bool) -> None:
        if self.use_audio and self.tts is not None:
            c = chime(rising)
            threading.Thread(target=self.player.play, args=(c.samples, c.sample_rate), daemon=True).start()

    # -- startup --------------------------------------------------------

    def load(self) -> None:
        cfg = self.cfg
        for name, cls in (("vad", Vad), ("stt", Stt)):
            try:
                setattr(self, name, cls(cfg))
                self.health[name] = ok()
            except (EngineUnavailable, Exception) as e:  # noqa: BLE001
                log.error("%s unavailable: %s", name, e)
                self.health[name] = unavailable(str(e))
        try:
            self.tts = Tts(cfg)
            self.health["tts"] = ok(f"loaded {cfg.tts_engine} voice: {cfg.tts_voice}")
        except EngineUnavailable as e:
            self.health["tts"] = disabled() if cfg.tts_engine == "none" else unavailable(str(e))
        except Exception as e:  # noqa: BLE001
            self.health["tts"] = unavailable(str(e))
        if cfg.mode == "push_to_talk":
            self.health["wake"] = disabled()
        elif cfg.wake_engine != "stt":
            self.health["wake"] = unavailable(f"wake_engine {cfg.wake_engine!r} not supported yet; use \"stt\"")
        else:
            self.health["wake"] = self.health["stt"] if self.stt else unavailable("needs speech-to-text")
        if self.vad and self.stt:
            self.pipeline = Pipeline(
                self.vad, self.stt, WakeMatcher(cfg.wake_words, cfg.name_variants), self._on_pipeline_report,
                mode=cfg.mode,
                timing=Timing(cfg.end_silence_ms, cfg.max_utterance_s, cfg.no_speech_timeout_s),
            )
        self.health["mic"] = unavailable("not started")

    def start_mic(self) -> None:
        if not self.use_audio or self.pipeline is None:
            self.health["mic"] = disabled() if not self.use_audio else unavailable("voice pipeline not loaded")
            return
        self.mic = Microphone(self.cfg.input_device, self._on_frame, self._on_mic_error)
        try:
            self.mic.start()
            self.health["mic"] = ok(self.cfg.input_device)
        except AudioError as e:
            self.health["mic"] = unavailable(str(e))

    def report_health(self) -> None:
        h = self.health
        self.emit({
            "report": "health",
            "mic": h.get("mic", unavailable("unknown")),
            "vad": h.get("vad", unavailable("unknown")),
            "wake": h.get("wake", unavailable("unknown")),
            "stt": h.get("stt", unavailable("unknown")),
            "tts": h.get("tts", unavailable("unknown")),
            "mode": self.cfg.mode,
            "input_device": self.cfg.input_device,
            "output_device": self.cfg.output_device,
        })

    # -- mic callbacks (mic thread) -------------------------------------

    def _on_frame(self, frame: np.ndarray) -> None:
        try:
            self._frames.put_nowait(frame)
        except queue.Full:
            # Worker is behind (STT running). Dropping idle audio is fine;
            # we never block the capture pipe.
            pass

    def _on_mic_error(self, msg: str) -> None:
        self.health["mic"] = unavailable(msg)
        self.emit({"report": "error", "component": "mic", "message": msg})
        self.report_health()

    # -- worker ---------------------------------------------------------

    def _worker(self) -> None:
        while not self._stop.is_set():
            try:
                action, cmd = self._ctl.get_nowait()
                self._apply(action, cmd)
                continue
            except queue.Empty:
                pass
            try:
                frame = self._frames.get(timeout=0.05)
            except queue.Empty:
                continue
            if frame is None:
                break
            if self.pipeline is not None:
                try:
                    self.pipeline.feed(frame)
                except Exception as e:  # noqa: BLE001
                    log.exception("pipeline error")
                    self.emit({"report": "error", "component": "pipeline", "message": str(e)})

    def _apply(self, action: str, cmd: dict) -> None:
        p = self.pipeline
        if p is None:
            self.emit({"report": "error", "component": "voice", "message": "voice pipeline not loaded"})
            return
        if action == "start_listening":
            if self.player.playing:
                self._cancel_speech.set()
                self.player.stop()
            if cmd.get("follow_up"):
                p.start_listening(timeout_s=FOLLOW_UP_TIMEOUT_S)
            else:
                p.start_listening()
        elif action == "stop_listening":
            p.stop_listening()
        elif action == "toggle_listening":
            if self.player.playing and p.state != "listening":
                self._cancel_speech.set()
                self.player.stop()
            p.toggle_listening()
        elif action == "cancel_listening":
            p.cancel_listening()
        elif action == "set_mode":
            mode = cmd.get("mode", "")
            if mode in ("push_to_talk", "wake_word", "continuous"):
                self.cfg.mode = mode
                p.set_mode(mode)
                self.health["wake"] = disabled() if mode == "push_to_talk" else self.health.get("stt", unavailable(""))
                self.report_health()

    # -- speaker --------------------------------------------------------

    def _speaker(self) -> None:
        while True:
            item = self._speech.get()
            if item is None:
                break
            text, uid, listen_after = item
            if self.tts is None:
                self.emit({"report": "speaking_finished", "utterance_id": uid, "interrupted": True})
                continue
            interrupted = self._speak_streamed(text, uid)
            self.emit({"report": "speaking_finished", "utterance_id": uid, "interrupted": interrupted})
            # Arc asked something: open the mic for the answer, no wake word needed.
            # Skipped if the user cut the speech off or more speech is queued
            # (the answer would be heard over Arc still talking).
            if listen_after and not interrupted and not self._more_speech_queued():
                self._ctl.put(("start_listening", {"follow_up": True}))

    def _speak_streamed(self, text: str, uid: str) -> bool:
        """Speak ``text`` sentence by sentence: sentence N+1 is rendered on a
        background thread while sentence N plays, so long replies start
        promptly and flow without gaps. Returns True if interrupted/failed."""
        sentences = split_sentences(text) or [text]
        self._cancel_speech.clear()
        rendered: queue.Queue = queue.Queue(maxsize=2)

        def render() -> None:
            for s in sentences:
                if self._cancel_speech.is_set():
                    break
                try:
                    rendered.put(self.tts.synthesize(s))
                except Exception as e:  # noqa: BLE001
                    rendered.put(e)
                    return
            rendered.put(None)

        threading.Thread(target=render, name="tts-render", daemon=True).start()
        started = False
        interrupted = False
        try:
            while True:
                sp = rendered.get()
                if sp is None:
                    break
                if isinstance(sp, Exception):
                    self.emit({"report": "error", "component": "tts", "message": str(sp)})
                    interrupted = True
                    break
                if self._cancel_speech.is_set():
                    interrupted = True
                    break
                if not started:
                    started = True
                    self.emit({"report": "speaking_started", "utterance_id": uid})
                    if self.pipeline:
                        self.pipeline.speaking = True
                try:
                    if self.use_audio and self.player.play(sp.samples, sp.sample_rate):
                        interrupted = True
                        break
                except AudioError as e:
                    self.emit({"report": "error", "component": "tts", "message": str(e)})
                    interrupted = True
                    break
        finally:
            self._cancel_speech.set()  # stop the renderer if we bailed early
            if self.pipeline:
                self.pipeline.speaking = False
        return interrupted

    def _more_speech_queued(self) -> bool:
        with self._speech.mutex:
            return any(item is not None for item in self._speech.queue)

    def _stop_speaking(self) -> None:
        while True:
            try:
                item = self._speech.get_nowait()
            except queue.Empty:
                break
            if item is not None:
                self.emit({"report": "speaking_finished", "utterance_id": item[1], "interrupted": True})
        self._cancel_speech.set()
        self.player.stop()

    # -- commands (main thread) -----------------------------------------

    def handle(self, cmd: dict) -> bool:
        """Handle one command. Returns False when the service should exit."""
        action = cmd.get("action")
        if action in ("start_listening", "stop_listening", "toggle_listening", "cancel_listening", "set_mode"):
            self._ctl.put((action, cmd))
        elif action == "speak":
            text = str(cmd.get("text", "")).strip()
            uid = str(cmd.get("utterance_id", ""))
            listen_after = bool(cmd.get("listen_after", False)) and self.cfg.follow_up
            if text:
                self._speech.put((text, uid, listen_after))
        elif action == "stop_speaking":
            self._stop_speaking()
        elif action == "reload":
            # Model reload needs a restart; the daemon restarts us on exit.
            log.info("reload requested; exiting for restart")
            return False
        elif action == "health":
            self.report_health()
        elif action == "shutdown":
            return False
        else:
            self.emit({"report": "error", "component": "protocol", "message": f"unknown action {action!r}"})
        return True

    def run(self, inp: IO[str] = sys.stdin) -> int:
        t0 = time.monotonic()
        self.load()
        self.start_mic()
        log.info("arc-voice %s ready in %.1fs (mode=%s)", __version__, time.monotonic() - t0, self.cfg.mode)
        self.report_health()
        worker = threading.Thread(target=self._worker, name="worker", daemon=True)
        speaker = threading.Thread(target=self._speaker, name="speaker", daemon=True)
        worker.start()
        speaker.start()
        try:
            for line in inp:
                line = line.strip()
                if not line:
                    continue
                try:
                    cmd = json.loads(line)
                    if not isinstance(cmd, dict):
                        raise ValueError("command must be a JSON object")
                except ValueError as e:
                    self.emit({"report": "error", "component": "protocol", "message": f"bad command: {e}"})
                    continue
                if not self.handle(cmd):
                    break
        finally:
            self._stop.set()
            if self.mic:
                self.mic.stop()
            self.player.stop()
            self._speech.put(None)
            self._frames.put(None)
        return 0
