"""Arc voice service.

Runs as a child process of the Arc daemon. Protocol: newline-delimited JSON.

* stdin  – one :class:`VoiceCommand` per line, e.g. ``{"action": "start_listening"}``
  (same shape as ``arc_proto::VoiceCommand``).
* stdout – one :class:`VoiceReport` per line, e.g. ``{"report": "transcript", ...}``
  (same shape as ``arc_proto::VoiceReport``).
* stderr – human-readable logs (collected by the daemon / journald).

Nothing else is ever written to stdout.
"""

__version__ = "0.1.0"
