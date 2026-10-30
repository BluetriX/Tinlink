#!/usr/bin/env python3
"""Synthesizes Tinlink's notification sound packs.

Sounds come in packs: sounds/<pack>/<sound>.wav in both clients, listed in
sounds.js. The packs in PACKS below are built here: "retro" from oscillators
and envelopes, "announcer" from the flite speech synthesizer (voice
cmu_us_slt, free license) and a radio filter plus reverb. No samples, no
recordings, so the files are ours (same license as the clients). The output
is deterministic; CI checks that the committed files match this script
(packs that need flite only where it is installed: apt install flite). A pack made some other way (e.g. CC0 recordings) only needs
its files and an entry in sounds.js; missing sounds fall back to "retro".

    python3 tools/sounds/generate.py            # writes into both clients
    python3 tools/sounds/generate.py --out DIR  # writes somewhere else
    python3 tools/sounds/generate.py --check    # compares, writes nothing
"""

import argparse
import math
import shutil
import struct
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

RATE = 44100
ROOT = Path(__file__).resolve().parents[2]
TARGETS = [ROOT / "apps/web/sounds", ROOT / "apps/desktop/ui/sounds"]
# Harmonics above this are left out, so square waves do not alias into hiss.
NYQUIST_LIMIT = 9000.0


def note_freq(name):
    """'A4' -> 440 Hz, 'C#5' -> 554 Hz."""
    names = {"C": -9, "D": -7, "E": -5, "F": -4, "G": -2, "A": 0, "B": 2}
    semis = names[name[0]]
    rest = name[1:]
    if rest.startswith("#"):
        semis += 1
        rest = rest[1:]
    elif rest.startswith("b"):
        semis -= 1
        rest = rest[1:]
    octave = int(rest)
    return 440.0 * 2 ** ((semis + 12 * (octave - 4)) / 12)


class Track:
    def __init__(self, seconds):
        self.buf = [0.0] * int(seconds * RATE)

    def tone(self, start, dur, freq, *, wave="pulse", duty=0.5, gain=1.0,
             attack=0.004, release=None, glide_to=None, vibrato=0.0,
             tremolo=0.0, fm_ratio=1.4, fm_index=0.0):
        """Adds one note. `freq` may glide to `glide_to` over the note."""
        release = dur * 0.6 if release is None else release
        n = int(dur * RATE)
        offset = int(start * RATE)
        phase = 0.0
        mod_phase = 0.0
        for i in range(n):
            t = i / RATE
            k = i / n
            f = freq if glide_to is None else freq * (glide_to / freq) ** k
            if vibrato:
                f *= 1 + vibrato * math.sin(2 * math.pi * 6.5 * t)
            phase += f / RATE
            if wave == "pulse":
                s = pulse(phase, f, duty)
            elif wave == "triangle":
                s = triangle(phase, f)
            elif wave == "fm":
                mod_phase += f * fm_ratio / RATE
                # The index decays, so the bell gets softer as it rings out.
                index = fm_index * math.exp(-t * 18)
                s = math.sin(2 * math.pi * phase + index * math.sin(2 * math.pi * mod_phase))
            else:
                s = math.sin(2 * math.pi * phase)
            env = envelope(t, dur, attack, release)
            if tremolo:
                env *= 1 - tremolo * (0.5 + 0.5 * math.sin(2 * math.pi * 14 * t))
            j = offset + i
            if j < len(self.buf):
                self.buf[j] += s * env * gain
        return self

    def echo(self, delay, feedback, mix):
        """A short feedback delay: the small "room" of old voice software."""
        d = int(delay * RATE)
        wet = [0.0] * len(self.buf)
        for i in range(len(self.buf)):
            back = wet[i - d] if i >= d else 0.0
            wet[i] = self.buf[i] + back * feedback
        self.buf = [dry + (w - dry) * mix for dry, w in zip(self.buf, wet)]
        return self

    def lowpass(self, cutoff):
        """One-pole low-pass that takes the edge off the square waves."""
        a = 1 - math.exp(-2 * math.pi * cutoff / RATE)
        y = 0.0
        for i, x in enumerate(self.buf):
            y += a * (x - y)
            self.buf[i] = y
        return self

    def samples(self, peak_db):
        peak = max(abs(s) for s in self.buf) or 1.0
        scale = 10 ** (peak_db / 20) / peak
        out = [s * scale for s in self.buf]
        # Fade the last 10 ms so a cut tail never clicks.
        fade = int(0.01 * RATE)
        for i in range(fade):
            out[-1 - i] *= i / fade
        return out


def pulse(phase, f, duty):
    """Band-limited pulse wave from its Fourier series."""
    s = 0.0
    k = 1
    while k * f < NYQUIST_LIMIT:
        s += math.sin(math.pi * k * duty) / k * math.cos(2 * math.pi * k * phase)
        k += 1
    return s * 4 / math.pi * 0.5


def triangle(phase, f):
    s = 0.0
    k = 1
    while k * f < NYQUIST_LIMIT:
        s += (-1) ** ((k - 1) // 2) / (k * k) * math.sin(2 * math.pi * k * phase)
        k += 2
    return s * 8 / (math.pi ** 2)


def envelope(t, dur, attack, release):
    if t < attack:
        return t / attack
    left = dur - t
    if left < release:
        # Exponential-ish decay towards the end of the note.
        return (left / release) ** 2
    return 1.0


# ------------------------------------------------------- pack "retro"
# Short, clear and synthetic. Up = something starts or someone arrives,
# down = something ends or someone goes.


def connected():
    tr = Track(0.55)
    for i, (n, d) in enumerate([("E5", 0.09), ("B5", 0.09), ("E6", 0.22)]):
        t0 = i * 0.075
        tr.tone(t0, d, note_freq(n), wave="pulse", duty=0.25, gain=0.6)
        tr.tone(t0, d, note_freq(n), wave="triangle", gain=0.5)
    return tr.lowpass(6000).echo(0.085, 0.3, 0.35)


def disconnected():
    tr = Track(0.6)
    for i, (n, d) in enumerate([("E6", 0.09), ("B5", 0.09), ("E5", 0.26)]):
        t0 = i * 0.075
        tr.tone(t0, d, note_freq(n), wave="pulse", duty=0.25, gain=0.6)
        tr.tone(t0, d, note_freq(n), wave="triangle", gain=0.5)
    return tr.lowpass(5000).echo(0.085, 0.3, 0.35)


def user_joined():
    tr = Track(0.4)
    tr.tone(0.0, 0.07, note_freq("A5"), glide_to=note_freq("D6"), wave="triangle", gain=0.8, release=0.03)
    tr.tone(0.065, 0.16, note_freq("A6"), wave="pulse", duty=0.125, gain=0.35)
    tr.tone(0.065, 0.16, note_freq("A6"), wave="triangle", gain=0.7)
    return tr.lowpass(7000).echo(0.07, 0.25, 0.3)


def user_left():
    tr = Track(0.45)
    tr.tone(0.0, 0.07, note_freq("D6"), glide_to=note_freq("A5"), wave="triangle", gain=0.8, release=0.03)
    tr.tone(0.065, 0.18, note_freq("D5"), wave="pulse", duty=0.125, gain=0.35)
    tr.tone(0.065, 0.18, note_freq("D5"), wave="triangle", gain=0.7)
    return tr.lowpass(6000).echo(0.07, 0.25, 0.3)


def moved():
    tr = Track(0.5)
    tr.tone(0.0, 0.14, 330, glide_to=1320, wave="pulse", duty=0.5, gain=0.5, release=0.04)
    tr.tone(0.15, 0.06, note_freq("E6"), wave="triangle", gain=0.8, release=0.04)
    tr.tone(0.22, 0.16, note_freq("B6"), wave="triangle", gain=0.7)
    return tr.lowpass(5500).echo(0.09, 0.3, 0.35)


def removed():
    tr = Track(0.6)
    tr.tone(0.0, 0.16, note_freq("A3"), wave="pulse", duty=0.125, gain=0.7, vibrato=0.01, release=0.05)
    tr.tone(0.18, 0.3, note_freq("E3"), wave="pulse", duty=0.125, gain=0.7, vibrato=0.015)
    return tr.lowpass(3500).echo(0.1, 0.25, 0.3)


def connection_lost():
    tr = Track(0.8)
    for i, n in enumerate(["A4", "F#4", "D#4"]):
        tr.tone(i * 0.14, 0.12 if i < 2 else 0.32, note_freq(n), wave="pulse", duty=0.5,
                gain=0.6, tremolo=0.5 if i == 2 else 0.0, release=0.04 if i < 2 else None)
    return tr.lowpass(3000).echo(0.11, 0.3, 0.3)


def message():
    tr = Track(0.55)
    tr.tone(0.0, 0.25, note_freq("G6"), wave="fm", fm_ratio=3.5, fm_index=2.2, gain=0.7, attack=0.002, release=0.22)
    tr.tone(0.085, 0.35, note_freq("D7"), wave="fm", fm_ratio=3.5, fm_index=1.8, gain=0.55, attack=0.002, release=0.32)
    return tr.lowpass(9000).echo(0.06, 0.2, 0.25)


def talking_muted():
    tr = Track(0.4)
    tr.tone(0.0, 0.13, note_freq("A5"), wave="pulse", duty=0.5, gain=0.6, release=0.06)
    tr.tone(0.17, 0.13, note_freq("E5"), wave="pulse", duty=0.5, gain=0.6, release=0.06)
    return tr.lowpass(2500)


# name -> (builder, peak level in dBFS); levels balance perceived loudness.
RETRO = {
    "connected": (connected, -6),
    "disconnected": (disconnected, -6),
    "user-joined": (user_joined, -7),
    "user-left": (user_left, -7),
    "moved": (moved, -6),
    "removed": (removed, -5),
    "connection-lost": (connection_lost, -8),
    "message": (message, -8),
    "talking-muted": (talking_muted, -13),
}



# --------------------------------------------------- pack "announcer"
# A computer voice says what happened, band-limited like a radio and with
# a big room behind it: the style of early 2000s voice chat programs.

VOICE = "slt"


def speak(text):
    """flite's speech for `text`, as floats at RATE."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "speech.wav"
        subprocess.run(["flite", "-voice", VOICE, "-t", text, "-o", str(path)], check=True)
        with wave.open(str(path), "rb") as w:
            rate = w.getframerate()
            data = w.readframes(w.getnframes())
    pcm = struct.unpack(f"<{len(data) // 2}h", data)
    return resample([x / 32768 for x in pcm], rate, RATE)


def resample(x, src, dst):
    """Linear interpolation; fine for speech that is low-passed afterwards."""
    n = int(len(x) * dst / src)
    out = []
    for i in range(n):
        pos = i * src / dst
        j = int(pos)
        frac = pos - j
        a = x[j]
        b = x[j + 1] if j + 1 < len(x) else 0.0
        out.append(a + (b - a) * frac)
    return out


def trim(x, threshold=0.01):
    """Cuts the silence flite leaves before and after the words."""
    loud = [i for i, v in enumerate(x) if abs(v) > threshold]
    return x[max(0, loud[0] - 200):loud[-1] + 200] if loud else x


def one_pole(x, cutoff, high=False):
    a = 1 - math.exp(-2 * math.pi * cutoff / RATE)
    y = 0.0
    out = []
    for v in x:
        y += a * (v - y)
        out.append(v - y if high else y)
    return out


def reverb(x, seconds, decay, mix):
    """Schroeder reverb: four combs in parallel, two all-passes in series."""
    tail = int(seconds * RATE)
    dry = x + [0.0] * tail
    wet = [0.0] * len(dry)
    for ms in (29.7, 37.1, 41.1, 43.7):
        d = int(ms / 1000 * RATE)
        g = 10 ** (-3 * (ms / 1000) / decay)  # -60 dB after `decay` seconds
        buf = [0.0] * len(dry)
        for i, v in enumerate(dry):
            buf[i] = v + (buf[i - d] * g if i >= d else 0.0)
            wet[i] += buf[i] * 0.25
    for ms, g in ((5.0, 0.7), (1.7, 0.7)):
        d = int(ms / 1000 * RATE)
        out = [0.0] * len(wet)
        for i, v in enumerate(wet):
            back = wet[i - d] if i >= d else 0.0
            prev = out[i - d] if i >= d else 0.0
            out[i] = -g * v + back + g * prev
        wet = out
    return [a + (b - a) * mix for a, b in zip(dry, wet)]


def announce(text):
    def build():
        x = trim(speak(text))
        for _ in range(3):
            x = one_pole(x, 450, high=True)
        x = one_pole(x, 5000)
        peak = max(abs(v) for v in x) or 1.0
        # A little drive, like an old radio speaker.
        x = [math.tanh(1.8 * v / peak) for v in x]
        tr = Track(0)
        tr.buf = reverb(x, 0.9, 1.1, 0.28)
        return tr
    build.needs = "flite"
    return build


ANNOUNCER = {
    "connected": (announce("Connected"), -4),
    "disconnected": (announce("Disconnected"), -4),
    "user-joined": (announce("User joined"), -4),
    "user-left": (announce("User left"), -4),
    "moved": (announce("You were moved"), -4),
    "removed": (announce("You were removed"), -4),
    "connection-lost": (announce("Connection lost"), -4),
    "message": (announce("New message"), -5),
    "talking-muted": (announce("You are muted"), -6),
}

PACKS = {"retro": RETRO, "announcer": ANNOUNCER}


def to_pcm(samples):
    return [max(-32767, min(32767, round(s * 32767))) for s in samples]


def write_wav(path, pcm):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(struct.pack(f"<{len(pcm)}h", *pcm))


def matches(path, pcm):
    """Same sound; one step of difference is allowed for libm rounding."""
    try:
        with wave.open(str(path), "rb") as w:
            data = w.readframes(w.getnframes())
            ok = (w.getnchannels(), w.getsampwidth(), w.getframerate()) == (1, 2, RATE)
    except (OSError, wave.Error):
        return False
    stored = struct.unpack(f"<{len(data) // 2}h", data)
    return ok and len(stored) == len(pcm) and all(abs(a - b) <= 1 for a, b in zip(stored, pcm))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, action="append", help="output directory (repeatable)")
    parser.add_argument("--check", action="store_true", help="only compare with the existing files")
    args = parser.parse_args()
    targets = args.out or TARGETS
    stale = []
    for pack, sounds in PACKS.items():
        for name, (build, peak_db) in sounds.items():
            tool = getattr(build, "needs", None)
            if tool and not shutil.which(tool):
                if not args.check:
                    sys.exit(f"pack {pack} needs {tool}")
                print(f"{pack}/{name}.wav  skipped, {tool} not installed")
                continue
            stale += build_sound(pack, name, build, peak_db, targets, args.check)
    if stale:
        for path in stale:
            print(f"out of date: {path}")
        sys.exit("run tools/sounds/generate.py and commit the result")


def build_sound(pack, name, build, peak_db, targets, check):
    """Writes (or with `check` compares) one sound; returns stale paths."""
    stale = []
    pcm = to_pcm(build().samples(peak_db))
    for base in targets:
        directory = base / pack
        path = directory / f"{name}.wav"
        if check:
            if not matches(path, pcm):
                stale.append(path)
            continue
        directory.mkdir(parents=True, exist_ok=True)
        write_wav(path, pcm)
    print(f"{pack}/{name}.wav  {len(pcm) / RATE:.2f} s")
    return stale


if __name__ == "__main__":
    main()
