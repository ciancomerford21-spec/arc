"""Audio I/O through PipeWire's ``pw-record`` / ``pw-play``.

Using the PipeWire CLI tools avoids native audio bindings in the venv and
lets PipeWire handle device selection, resampling and format conversion.
Capture is always 16 kHz mono float32; playback accepts any rate.

Export ``ARC_VOICE_DEBUG`` or run with ``python -m arc_voice -v`` for debug
logs from the voice service. Both the daemon and ``arc-voice --say`` use the
same default device probing so standalone and daemon-started runs agree.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import threading
from collections.abc import Callable

import numpy as np

from .engines import FRAME, SAMPLE_RATE

_log = __import__("logging").getLogger(__name__)


class AudioError(RuntimeError):
    pass


def _probe() -> dict | None:
    """Best-effort PipeWire device list. Returns None if pw-dump/JSON parsing fails."""
    exe = shutil.which("pw-dump")
    if not exe:
        return None
    try:
        out = subprocess.run([exe, "-n", "1"], capture_output=True, text=True, timeout=5).stdout
        tree = json.loads(out)
    except Exception:
        return None
    return tree


def _default_output_device() -> str:
    """Best-effort default output device (won't raise on PipeWire absences)."""
    try:
        p = _probe()
        if not p:
            return DEFAULT_OUTPUT
        # Prefer a non-monitor sink if one exists.
        for node in p:
            props = node.get("props") or {}
            obj = props.get("object") or {}
            if obj.get("class") != "PipeWire:Interface:Node":
                continue
            media = props.get("media") or {}
            name = media.get("name") or ""
            if "none" in name.lower() or "monitor" in name.lower():
                continue
            if not media.get("direction") or "output" in str(media.get("direction", "")).lower():
                return name
        return DEFAULT_OUTPUT
    except Exception:
        return DEFAULT_OUTPUT


def _default_input_device() -> str:
    """Best-effort default input device (won't raise on PipeWire absences)."""
    try:
        p = _probe()
        if not p:
            return DEFAULT_INPUT
        for node in p:
            props = node.get("props") or {}
            obj = props.get("object") or {}
            if obj.get("class") != "PipeWire:Interface:Node":
                continue
            media = props.get("media") or {}
            name = media.get("name") or ""
            if "none" in name.lower() or "monitor" in name.lower():
                continue
            if not media.get("direction") or "input" in str(media.get("direction", "")).lower():
                return name
        return DEFAULT_INPUT
    except Exception:
        return DEFAULT_INPUT


def _target_args(device: str) -> list[str]:
    return [] if device in ("", "default") else ["--target", device]


class Microphone:
    """Continuous capture; calls ``on_frame`` with FRAME-sample float32 arrays
    from a reader thread. ``on_error`` is called once if capture dies."""

    def __init__(self, device: str, on_frame: Callable[[np.ndarray], None], on_error: Callable[[str], None]):
        self.device = device
        self._on_frame = on_frame
        self._on_error = on_error
        self._proc: subprocess.Popen | None = None
        self._thread: threading.Thread | None = None
        self._stopping = False

    def start(self) -> None:
        exe = shutil.which("pw-cat")
        if not exe:
            raise AudioError("pw-cat not found (install pipewire)")
        cmd = [
            exe, "--record", "--raw", "--rate", str(SAMPLE_RATE), "--channels", "1", "--format", "f32",
            "--latency", "32ms", "--media-role", "Communication",
            "-P", '{ media.name = "Arc voice" }', *_target_args(self.device), "-",
        ]
        self._proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self._stopping = False
        self._thread = threading.Thread(target=self._read, name="mic", daemon=True)
        self._thread.start()

    def _read(self) -> None:
        assert self._proc and self._proc.stdout
        need = FRAME * 4
        buf = b""
        while True:
            chunk = self._proc.stdout.read(need - len(buf))
            if not chunk:
                break
            buf += chunk
            if len(buf) < need:
                continue
            self._on_frame(np.frombuffer(buf[:need], dtype=np.float32).copy())
            buf = buf[need:]

    def stop(self) -> None:
        if self._proc is None:
            return
        self._stopping = True
        try:
            self._proc.terminate()
            self._proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self._proc.kill()


class Player:
    """Playback via ``pw-cat --playback``. ``play`` blocks until the samples
    are handed off to PipeWire; use it from the speaker thread only."""

    def __init__(self, device: str | None):
        self.device = device or DEFAULT_OUTPUT
        self.playing = False

    def play(self, samples: np.ndarray, rate: int) -> bool:
        """Write samples to the speaker; returns True if the user stopped playback."""
        if not self.device or not samples.size:
            return False
        self.playing = True
        try:
            exe = shutil.which("pw-cat") or shutil.which("pw-play")
            if not exe:
                raise AudioError("pw-cat/pw-play not found (install pipewire)")
            proc = subprocess.Popen(
                [exe, "--playback", "--raw", "--rate", str(rate), "--channels", "1", "--format", "f32",
                 *_target_args(self.device), "-"],
                stdin=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
            )
            if proc.stdin is None:
                raise AudioError("pw-cat stdin vanished")
            bytes_written = proc.stdin.write((samples.astype(np.float32).tobytes()))
            proc.stdin.close()
            # Brief wait: if the user hit the stop key, the process may already be gone.
            try:
                proc.wait(timeout=0.2)
            except subprocess.TimeoutExpired:
                pass
        except OSError as e:
            raise AudioError(f"pw-cat failed: {e}") from e
        finally:
            self.playing = False
        return False

    def stop(self) -> None:
        try:
            subprocess.run(["pw-cli", "destroy", f"device:{self.device}"], capture_output=True, timeout=2)
        except Exception:
            pass
        self.playing = False


def chime(rising: bool) -> np.ndarray:
    """Two-tone-ish chime at 16 kHz for the listening_started / listening_stopped reports."""
    dur = 0.12
    n = int(SAMPLE_RATE * dur)
    t = np.arange(n, dtype=np.float32) / SAMPLE_RATE
    tone = np.sin(2 * np.pi * (880 if rising else 660) * t) * np.exp(-t * 18) * 0.5
    return tone
