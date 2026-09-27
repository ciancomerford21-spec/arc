from pathlib import Path

import pytest

from arc_voice.config import VoiceConfig
from arc_voice.wake import WakeMatcher

M = WakeMatcher(["hey arc", "okay arc", "ok arc"], ["arc", "ark"])


@pytest.mark.parametrize(
    "text, phrase, command",
    [
        ("Hey Arc, lock the screen.", "hey arc", "lock the screen."),
        ("hey ark lock the screen", "hey ark", "lock the screen"),
        ("Okay Arc.", "ok arc", ""),
        ("Ok, arc. Lock the screen.", "ok arc", "Lock the screen."),
        ("OK, Arc, what's the weather?", "ok arc", "what's the weather?"),
        ("Arc, mute", "arc", "mute"),
        ("Um, hey Arc open firefox", "hey arc", "open firefox"),
        ("Hey Arc", "hey arc", ""),
        # Real Moonshine output for "Hey Arc, what time is it?": name run into the next word.
        ("Hey, article time, is it?", "hey article", "time, is it?"),
        ("Okay, archer volume up", "ok archer", "volume up"),
    ],
)
def test_matches(text, phrase, command):
    m = M.match(text)
    assert m is not None, text
    assert m.phrase == phrase
    assert m.command == command


@pytest.mark.parametrize(
    "text",
    ["", "lock the screen", "the arc of history", "hey there", "archive this", "search for arc welding",
     # Overheard conversation from a live session: must not wake.
     "Take the light out", "I think I will now lecture them in.", "hey what time is it", "arctic weather is mad",
     "ok are we going", "hey around five we leave", "okay artist mode"],
)
def test_no_match(text):
    assert M.match(text) is None


def test_config_defaults_and_toml(tmp_path: Path):
    assert VoiceConfig.load(tmp_path / "missing.toml").mode == "wake_word"
    p = tmp_path / "config.toml"
    p.write_text(
        '[general]\nname_variants = ["Jarvis"]\n'
        '[voice]\nmode = "push_to_talk"\nspeech_rate = 9\nunknown_key = 1\nwake_words = [" Hey Jarvis "]\n'
    )
    c = VoiceConfig.load(p)
    assert c.mode == "push_to_talk"
    assert c.speech_rate == 2.0  # clamped
    assert c.wake_words == ["hey jarvis"]
    assert c.name_variants == ["jarvis"]


def test_config_rejects_bad_mode(tmp_path: Path):
    p = tmp_path / "config.toml"
    p.write_text('[voice]\nmode = "always"\n')
    with pytest.raises(ValueError):
        VoiceConfig.load(p)
