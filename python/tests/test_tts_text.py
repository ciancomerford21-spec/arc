import pytest

from arc_voice.engines import EngineUnavailable, kokoro_speaker_id, split_sentences


def test_split_sentences():
    assert split_sentences("Opened VS Code. Your CPU is at 4%. Anything else?") == [
        "Opened VS Code.", "Your CPU is at 4%. Anything else?"
    ]
    assert split_sentences("It's 3:03 PM.") == ["It's 3:03 PM."]
    # Decimals, versions and abbreviations mid-sentence don't split.
    assert split_sentences("Version 1.2.3 is out. Volume is 0.8 now.") == ["Version 1.2.3 is out.", "Volume is 0.8 now."]
    assert split_sentences("") == []


def test_split_sentences_merges_by_word_count_not_length():
    # "Anything else?" is 13 characters but two words. Under the old
    # character threshold it became its own clip, so the same reply played as
    # three clips when written in words and two when written as "4%".
    assert split_sentences("Your CPU is at four percent. Anything else?") == [
        "Your CPU is at four percent. Anything else?"
    ]
    # A genuinely separate sentence still gets its own clip.
    assert split_sentences("Opened VS Code. The build is still running.") == [
        "Opened VS Code.", "The build is still running."
    ]


def test_kokoro_speakers():
    assert kokoro_speaker_id("bf_emma") == 7
    assert kokoro_speaker_id("AF_Bella") == 1
    assert kokoro_speaker_id("3") == 3
    assert kokoro_speaker_id("") == 0
    with pytest.raises(EngineUnavailable):
        kokoro_speaker_id("not_a_voice")
