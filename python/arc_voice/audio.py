"""Audio I/O through PipeWire's ``pw-record`` / ``pw-play``.

Using the PipeWire CLI tools avoids native audio bindings in the venv and
lets PipeWire handle device selection, resampling and format conversion.
Capture is always 16 kHz mono float32; playback accepts any rate.
"""

from __future__ import annotations

import shutil
import subprocess
import threading
from collections.abc import Callable

import numpy as np

from .engines import FRAME, SAMPLE_RATE


class AudioError(RuntimeError):
    pass


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
        exe = shutil.which("pw-record")
        if not exe:
            raise AudioError("pw-record not found (install pipewire)")
        cmd = [
            exe, "--rate", str(SAMPLE_RATE), "--channels", "1", "--format", "f32",
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
            if len(buf) == need:
                self._on_frame(np.frombuffer(buf, dtype=np.float32).copy())
                buf = b""
        if not self._stopping:
            err = b""
            if self._proc.stderr:
                err = self._proc.stderr.read() or b""
            self._on_error(f"microphone capture stopped: {err.decode(errors='replace').strip() or 'pw-record exited'}")

    def stop(self) -> None:
        self._stopping = True
        if self._proc and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        self._proc = None


class Player:
    """Plays one clip at a time. ``stop()`` interrupts the current clip."""

    def __init__(self, device: str):
        self.device = device
        self._proc: subprocess.Popen | None = None
        self._lock = threading.Lock()
        self._interrupted = False

    def play(self, samples: np.ndarray, sample_rate: int) -> bool:
        """Blocking. Returns True if playback was interrupted."""
        exe = shutil.which("pw-play")
        if not exe:
            raise AudioError("pw-play not found (install pipewire)")
        cmd = [
            exe, "--rate", str(sample_rate), "--channels", "1", "--format", "f32",
            "--media-role", "Assistant", "-P", '{ media.name = "Arc" }', *_target_args(self.device), "-",
        ]
        with self._lock:
            self._interrupted = False
            self._proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stderr=subprocess.DEVNULL)
            proc = self._proc
        try:
            assert proc.stdin
            proc.stdin.write(np.ascontiguousarray(samples, dtype=np.float32).tobytes())
            proc.stdin.close()
        except (BrokenPipeError, ValueError):
            pass
        proc.wait()
        with self._lock:
            self._proc = None
            return self._interrupted

    def stop(self) -> bool:
        with self._lock:
            if self._proc and self._proc.poll() is None:
                self._interrupted = True
                self._proc.terminate()
                return True
        return False

    @property
    def playing(self) -> bool:
        with self._lock:
            return self._proc is not None and self._proc.poll() is None
