"""Wake phrase matching on transcripts.

Wake detection is transcript-based (``voice.wake_engine = "stt"``): each
speech segment is transcribed and checked for a wake phrase at its start.
Speech recognisers vary in how they spell the name ("Hey Arc", "hey, ark",
"Okay Arc."), so matching is done on normalised tokens, and every wake phrase
that contains a name is expanded to every configured spelling of that name.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

_TOKEN = re.compile(r"[a-z0-9']+")
FILLERS = {"uh", "um", "erm", "so", "oh", "ah", "hmm"}
#: Spellings recognisers use interchangeably.
CANON = {"okay": "ok", "o.k.": "ok", "hei": "hey", "hay": "hey"}


def tokens(text: str) -> list[str]:
    return [CANON.get(t, t) for t in _TOKEN.findall(text.lower().replace("’", "'"))]


@dataclass(frozen=True)
class WakeMatch:
    phrase: str
    #: What followed the wake phrase ("" when the wake phrase was said alone).
    command: str


class WakeMatcher:
    def __init__(self, wake_words: list[str], name_variants: list[str]):
        names = [n for n in (tokens(v) for v in name_variants) if len(n) == 1]
        name_set = {n[0] for n in names}
        phrases: set[tuple[str, ...]] = set()
        for w in wake_words:
            t = tuple(tokens(w))
            if not t:
                continue
            phrases.add(t)
            # "hey arc" -> "hey ark", ...
            for i, tok in enumerate(t):
                if tok in name_set:
                    for n in name_set:
                        phrases.add(t[:i] + (n,) + t[i + 1 :])
        # A bare name at the start also counts ("Arc, lock the screen").
        for n in name_set:
            phrases.add((n,))
        # Longest first so "hey arc" wins over "arc".
        self.phrases = sorted(phrases, key=len, reverse=True)

    def match(self, transcript: str) -> WakeMatch | None:
        """Return the match if ``transcript`` starts with a wake phrase."""
        # Keep original words (for the command) aligned with normalised tokens.
        words = transcript.split()
        norm = [tokens(w) for w in words]
        flat: list[tuple[int, str]] = [(i, t) for i, ts in enumerate(norm) for t in ts]
        start = 0
        while start < len(flat) and flat[start][1] in FILLERS and start < 2:
            start += 1
        toks = [t for _, t in flat[start:]]
        for p in self.phrases:
            if tuple(toks[: len(p)]) == p:
                end = start + len(p)
                if end < len(flat):
                    word_idx = flat[end][0]
                    # If the next token shares a word with the phrase's last
                    # token (e.g. "arc's"), start after that word.
                    if word_idx == flat[end - 1][0]:
                        word_idx += 1
                    rest = " ".join(words[word_idx:])
                else:
                    rest = ""
                rest = rest.strip().lstrip(",.;:!?-– ").strip()
                return WakeMatch(" ".join(p), rest)
        return None
