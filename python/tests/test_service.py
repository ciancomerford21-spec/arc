"""Protocol tests: run the real service process with --no-audio."""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]


def spawn(env_extra=None):
    env = dict(os.environ, PYTHONPATH=str(ROOT), **(env_extra or {}))
    return subprocess.Popen(
        [sys.executable, "-m", "arc_voice", "--no-audio"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
    )


def read_report(p, want=None, timeout=30.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        line = p.stdout.readline()
        if not line:
            raise AssertionError(f"process exited: {p.stderr.read()}")
        r = json.loads(line)
        if want is None or r["report"] == want:
            return r
    raise AssertionError(f"timeout waiting for {want}")


def test_health_first_and_stdout_is_pure_json(stt):
    p = spawn()
    try:
        h = read_report(p)
        assert h["report"] == "health"
        assert h["stt"]["status"] == "ok"
        assert h["mic"]["status"] == "disabled"
        assert h["mode"] in ("push_to_talk", "wake_word", "continuous")
        p.stdin.write('not json\n{"action":"frobnicate"}\n{"action":"health"}\n')
        p.stdin.flush()
        e1 = read_report(p)
        e2 = read_report(p)
        assert e1["report"] == "error" and e1["component"] == "protocol"
        assert e2["report"] == "error" and "frobnicate" in e2["message"]
        assert read_report(p)["report"] == "health"
    finally:
        p.stdin.close()
        p.wait(timeout=10)
    assert p.returncode == 0


def test_speak_reports_lifecycle(tts):
    p = spawn()
    try:
        read_report(p, "health")
        p.stdin.write(json.dumps({"action": "speak", "text": "Done.", "utterance_id": "u1"}) + "\n")
        p.stdin.flush()
        s = read_report(p)
        f = read_report(p)
        assert s == {"report": "speaking_started", "utterance_id": "u1"}
        assert f["report"] == "speaking_finished" and f["utterance_id"] == "u1"
    finally:
        p.stdin.write('{"action":"shutdown"}\n')
        p.stdin.flush()
        p.wait(timeout=10)
    assert p.returncode == 0


def test_missing_models_degrade_gracefully(tmp_path):
    p = spawn({"ARC_DATA_DIR": str(tmp_path), "ARC_CONFIG_DIR": str(tmp_path)})
    try:
        h = read_report(p)
        assert h["stt"]["status"] == "unavailable"
        assert "fetch-models" in h["stt"]["detail"]
        p.stdin.write('{"action":"start_listening"}\n')
        p.stdin.flush()
        assert read_report(p)["report"] == "error"
    finally:
        p.stdin.close()
        p.wait(timeout=10)


def test_transcribe_cli(stt, tmp_path, say):
    import wave

    import numpy as np

    wav = tmp_path / "a.wav"
    with wave.open(str(wav), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes((np.clip(say("Lock the screen."), -1, 1) * 32767).astype(np.int16).tobytes())
    out = subprocess.run(
        [sys.executable, "-m", "arc_voice", "--transcribe", str(wav)],
        capture_output=True, text=True, env=dict(os.environ, PYTHONPATH=str(ROOT)), timeout=60,
    )
    assert out.returncode == 0, out.stderr
    assert "lock the screen" in json.loads(out.stdout)["text"].lower()
