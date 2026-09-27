"""Pipeline tests on synthesised speech with the real VAD and STT models."""

import numpy as np
import pytest

from arc_voice.pipeline import Pipeline, Timing
from arc_voice.wake import WakeMatcher

from .conftest import frames, silence

WAKE = WakeMatcher(["hey arc", "okay arc"], ["arc", "ark"])


def run(pipeline: Pipeline, audio: np.ndarray, reports: list[dict]) -> list[dict]:
    for f in frames(audio):
        pipeline.feed(f)
    return [r for r in reports if r["report"] != "level"]


def make(vad_factory, stt, mode="wake_word", **timing):
    reports: list[dict] = []
    p = Pipeline(vad_factory(), stt, WAKE, reports.append, mode=mode, timing=Timing(**timing))
    return p, reports


def transcripts(rs):
    return [r["text"].lower().rstrip(".") for r in rs if r["report"] == "transcript"]


def norm(s: str) -> str:
    return "".join(c for c in s.lower() if c.isalnum() or c == " ").strip()


def test_stt_transcribes_synthesised_speech(stt, say):
    tr = stt.transcribe(say("Set the volume to forty percent."))
    assert "volume" in tr.text.lower()
    assert tr.audio_ms > 1000


def test_wake_and_command_in_one_breath(vad_factory, stt, say):
    p, reports = make(vad_factory, stt)
    rs = run(p, np.concatenate([silence(0.5), say("Hey Arc, lock the screen."), silence(1.2)]), reports)
    kinds = [r["report"] for r in rs]
    assert kinds[0] == "wake_detected", rs
    assert norm(transcripts(rs)[0]) == "lock the screen"
    assert p.state == "idle"


def test_wake_then_pause_then_command(vad_factory, stt, say):
    p, reports = make(vad_factory, stt)
    audio = np.concatenate([
        silence(0.4), say("Hey Arc."), silence(1.0), say("Open the file manager."), silence(1.2),
    ])
    rs = run(p, audio, reports)
    kinds = [r["report"] for r in rs]
    assert kinds[:2] == ["wake_detected", "listening_started"], kinds
    assert "transcribing" in kinds
    assert "file manager" in norm(transcripts(rs)[0])


def test_speech_without_wake_word_is_ignored(vad_factory, stt, say):
    p, reports = make(vad_factory, stt)
    rs = run(p, np.concatenate([silence(0.3), say("Lock the screen."), silence(1.2)]), reports)
    assert rs == []


def test_continuous_mode_passes_everything(vad_factory, stt, say):
    p, reports = make(vad_factory, stt, mode="continuous")
    rs = run(p, np.concatenate([silence(0.3), say("Lock the screen."), silence(1.2)]), reports)
    assert norm(transcripts(rs)[0]) == "lock the screen"


def test_push_to_talk(vad_factory, stt, say):
    p, reports = make(vad_factory, stt, mode="push_to_talk")
    run(p, np.concatenate([silence(0.3), say("Hey Arc, mute."), silence(1.0)]), reports)
    assert reports == [], "push-to-talk must ignore speech until started"
    p.start_listening()
    rs = run(p, np.concatenate([silence(0.2), say("Mute the audio."), silence(1.2)]), reports)
    kinds = [r["report"] for r in rs]
    assert kinds[0] == "listening_started"
    assert "mute the audio" in norm(transcripts(rs)[0])
    assert p.state == "idle"


def test_push_to_talk_manual_stop(vad_factory, stt, say):
    p, reports = make(vad_factory, stt, mode="push_to_talk", end_silence_ms=5000)
    p.start_listening()
    run(p, np.concatenate([silence(0.2), say("Pause the music."), silence(0.3)]), reports)
    assert p.state == "listening"
    p.stop_listening()
    assert "pause the music" in norm(transcripts(reports)[0])


def test_no_speech_timeout(vad_factory, stt):
    p, reports = make(vad_factory, stt, mode="push_to_talk", no_speech_timeout_s=1.0)
    p.start_listening()
    rs = run(p, silence(1.5), reports)
    assert [r["report"] for r in rs] == ["listening_started", "no_speech"]
    assert p.state == "idle"


def test_levels_emitted_while_listening(vad_factory, stt, say):
    p, reports = make(vad_factory, stt, mode="push_to_talk")
    p.start_listening()
    for f in frames(say("Hello there.")):
        p.feed(f)
    levels = [r["rms"] for r in reports if r["report"] == "level"]
    assert levels and max(levels) > 0.01 and all(0 <= v <= 1 for v in levels)


def test_while_speaking_only_wake_phrase_counts(vad_factory, stt, say):
    p, reports = make(vad_factory, stt, mode="continuous")
    p.speaking = True
    rs = run(p, np.concatenate([silence(0.3), say("Lock the screen."), silence(1.2)]), reports)
    assert transcripts(rs) == [], "own speech / background must not become commands"
    rs = run(p, np.concatenate([say("Hey Arc, stop."), silence(1.2)]), reports)
    assert any(r["report"] == "wake_detected" for r in rs)


@pytest.mark.parametrize("utter", ["Okay Arc, lock the screen.", "OK Arc, lock the screen.", "Hey Ark, lock the screen."])
def test_wake_variants(vad_factory, stt, say, utter):
    p, reports = make(vad_factory, stt)
    rs = run(p, np.concatenate([silence(0.3), say(utter), silence(1.2)]), reports)
    assert rs and rs[0]["report"] == "wake_detected", rs
    assert "lock the screen" in norm(" ".join(transcripts(rs))), rs
