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
import time
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
            try:
                chunk = self._proc.stdout.read(need - len(buf))
            except (ValueError, OSError) as e:
                # The pipe died (process gone, or fd closed underneath us).
                if not self._stopping:
                    self._on_error(f"microphone stream failed: {e}")
                return
            if not chunk:
                if self._stopping:
                    return
                # Clean EOF from pw-cat: it exited. Surface it instead of
                # dying silently, otherwise the service keeps reporting
                # "ready" while hearing nothing.
                code = self._proc.poll()
                detail = ""
                if code is not None and code != 0:
                    # pw-cat's last stderr line usually names the real problem
                    # (device busy, stream suspended, driver gone).
                    try:
                        err = ""
                        if self._proc.stderr is not None:
                            err = self._proc.stderr.read().decode(errors="replace").strip()
                    except (ValueError, OSError):
                        err = ""
                    if err:
                        detail = f": {err.splitlines()[-1]}"
                    self._on_error(f"microphone stream ended unexpectedly (pw-cat exit {code}){detail}")
                else:
                    self._on_error("microphone stream ended unexpectedly")
                return
            buf += chunk
            if len(buf) < need:
                continue
            self._on_frame(np.frombuffer(buf[:need], dtype=np.float32).copy())
            buf = buf[need:]

    def is_alive(self) -> bool:
        """False once the capture process has exited (so it can be restarted)."""
        return self._proc is not None and self._proc.poll() is None

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
    """Playback via ``pw-cat --playback``.

    One ``pw-cat`` stream is opened for the lifetime of the player rather than
    per clip: a fresh stream per sentence overlaps, because the new stream
    starts before the old one has finished playing. ``play`` then blocks until
    the audio has actually been consumed, so sentences run one after another
    instead of on top of each other.
    """

    def __init__(self, device: str | None):
        self.device = device or DEFAULT_OUTPUT
        self.playing = False
        self._proc: subprocess.Popen | None = None
        self._lock = threading.Lock()
        self._play_lock = threading.Lock()
        self._rate = 0
        self._interrupted = False

    def _ensure_stream(self) -> subprocess.Popen:
        """Open the playback stream if it isn't already running."""
        with self._lock:
            if self._proc is not None and self._proc.poll() is None:
                return self._proc
            exe = shutil.which("pw-cat") or shutil.which("pw-play")
            if not exe:
                raise AudioError("pw-cat/pw-play not found (install pipewire)")
            self._proc = subprocess.Popen(
                [exe, "--playback", "--raw", "--rate", str(self._rate), "--channels", "1", "--format", "f32",
                 # A small latency is essential: with pw-cat's default buffer,
                 # the next sentence is queued into PipeWire while the current
                 # one is still draining, and the two play over each other.
                 "--latency", "40ms", *_target_args(self.device), "-"],
                stdin=subprocess.PIPE, bufsize=0,
            )
            return self._proc

    def play(self, samples: np.ndarray, rate: int) -> bool:
        """Write samples to the speaker and block until they finish playing.

        Returns True if the user stopped playback. Serialised against other
        callers (the listening chime runs on its own thread), so two sounds
        never talk over each other.
        """
        if not self.device or not samples.size:
            return False
        with self._play_lock:
            return self._play_locked(samples, rate)

    def _play_locked(self, samples: np.ndarray, rate: int) -> bool:
        self.playing = True
        try:
            if rate != self._rate or self._proc is None or self._proc.poll() is not None:
                # A new sample rate needs a new stream; drop the old one first
                # so two streams are never live at once.
                self._close_stream()
                self._rate = rate
            proc = self._ensure_stream()
            if proc.stdin is None:
                raise AudioError("pw-cat stdin vanished")
            # Write in chunks so a long sentence stays interruptible: a single
            # large write() blocks until pw-cat drains it, which would delay
            # stop() by the whole clip.
            payload = memoryview(samples.astype(np.float32).tobytes())
            chunk_bytes = SAMPLE_RATE * 4  # ~1 s of float32 mono
            for off in range(0, len(payload), chunk_bytes):
                if self._interrupted:
                    self._close_stream()
                    return True
                try:
                    proc.stdin.write(payload[off : off + chunk_bytes])
                    proc.stdin.flush()
                except (ValueError, OSError) as e:
                    # stop() closed the stream underneath us — that's a normal
                    # interruption, not a playback failure.
                    if self._interrupted:
                        return True
                    raise AudioError(f"pw-cat stdin failed: {e}") from e
            # pw-cat keeps running while its stdin is open (we reuse the stream
            # across sentences), so waiting for it to exit would never work.
            # Instead wait for the clip's own duration: the samples are
            # written before we get here, and PipeWire plays them at real-time
            # rate, so duration + a small drain margin is the finish line.
            # The margin is deliberately small — a large one leaves audible
            # dead air between sentences, which reads as a stutter.
            limit = samples.size / float(rate) + 0.12
            deadline = time.monotonic() + limit
            while time.monotonic() < deadline:
                if self._interrupted:
                    self._close_stream()
                    return True
                time.sleep(0.01)
            return self._interrupted
        except (OSError, BrokenPipeError) as e:
            raise AudioError(f"pw-cat failed: {e}") from e
        finally:
            self.playing = False

    def _close_stream(self) -> None:
        with self._lock:
            proc, self._proc = self._proc, None
        if proc is None:
            return
        try:
            if proc.stdin and not proc.stdin.closed:
                proc.stdin.close()
        except (OSError, BrokenPipeError):
            pass
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass

    def stop(self) -> None:
        """Cut playback off now: kill the stream, not the device.

        The old implementation ran `pw-cli destroy device:<name>`, which takes
        the whole output device offline and can silence other players.
        """
        self._interrupted = True
        self._close_stream()
        self.playing = False

    def close(self) -> None:
        self._interrupted = False
        self._close_stream()


def chime(rising: bool) -> np.ndarray:
    """Two-tone-ish chime at 16 kHz for the listening_started / listening_stopped reports."""
    dur = 0.12
    n = int(SAMPLE_RATE * dur)
    t = np.arange(n, dtype=np.float32) / SAMPLE_RATE
    tone = np.sin(2 * np.pi * (880 if rising else 660) * t) * np.exp(-t * 18) * 0.5
    return tone
