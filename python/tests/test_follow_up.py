"""Follow-up listening: after Arc asks something, the mic opens without a wake word."""

import io
import json
import queue
import threading
import time

import numpy as np

from arc_voice.config import VoiceConfig
from arc_voice.service import Service


class FakeTts:
    def synthesize(self, text):
        class S:
            samples = np.zeros(10, dtype=np.float32)
            sample_rate = 16000
        return S()


class FakePlayer:
    def __init__(self, interrupted=False):
        self.interrupted = interrupted
        self.playing = False

    def play(self, samples, rate):
        return self.interrupted

    def stop(self):
        return False


def run_speak(cmd: dict, *, follow_up=True, interrupted=False):
    cfg = VoiceConfig()
    cfg.follow_up = follow_up
    svc = Service(cfg, out=io.StringIO(), use_audio=True)
    svc.tts = FakeTts()
    svc.player = FakePlayer(interrupted)
    t = threading.Thread(target=svc._speaker, daemon=True)
    t.start()
    svc.handle({"action": "speak", "utterance_id": "u1", **cmd})
    svc._speech.put(None)
    t.join(timeout=5)
    reports = [json.loads(line) for line in svc._out.getvalue().splitlines()]
    ctl = []
    while True:
        try:
            ctl.append(svc._ctl.get_nowait())
        except queue.Empty:
            break
    return reports, ctl


def test_listens_after_question():
    reports, ctl = run_speak({"text": "Which app did you mean?", "listen_after": True})
    assert [r["report"] for r in reports] == ["speaking_started", "speaking_finished"]
    assert ctl == [("start_listening", {"follow_up": True})]


def test_no_listen_without_flag():
    _, ctl = run_speak({"text": "Switched to workspace 3."})
    assert ctl == []


def test_no_listen_when_disabled_in_config():
    _, ctl = run_speak({"text": "Confirm?", "listen_after": True}, follow_up=False)
    assert ctl == []


def test_no_listen_when_speech_was_interrupted():
    _, ctl = run_speak({"text": "Confirm?", "listen_after": True}, interrupted=True)
    assert ctl == []
