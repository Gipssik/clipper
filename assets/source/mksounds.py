"""Regenerates the replay-saved sounds in assets/sounds/. Run it after changing a voice:

    python assets/source/mksounds.py

Produces:
    assets/sounds/chime.wav     two bell notes a fifth apart, rising        (the default)
    assets/sounds/pop.wav       one soft rising blip, over in a quarter of a second
    assets/sounds/sparkle.wav   a quick four-note arpeggio with a glassy tail

Every sound is synthesised here from sines, so there is no sample to license and nothing to
download. The design constraints, in order:

  - It plays over a game, so it must not share the game's weight. Everything sits between
    about 800 Hz and 2.4 kHz, clear of the bass and low mids where gunfire and engines live, and
    below the 3-4 kHz band the ear is most sensitive to, which is where "pleasant" turns "shrill".
  - No click. A 4 ms raised-cosine attack, and each tail is faded to zero before the file ends.
  - Upper partials die faster than the fundamental, the way a struck bell or bar does. A tone whose
    overtones hold is a synth pad; one whose overtones fall away is an object being struck.
  - A small reverb gives it a room, which is most of what separates a notification from a beep.

ffmpeg's EBU R128 meter measures each result and the gain is set so they all peak at the same
momentary loudness: switching sound in Settings changes the character, not the volume.
"""
import math, os, re, struct, subprocess, wave

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FFMPEG = os.path.join(ROOT, 'ffmpeg-bin', 'ffmpeg.exe')
OUT = os.path.join(ROOT, 'assets', 'sounds')

RATE = 48000
# Momentary (400 ms) loudness each sound peaks at. Game audio sits around -14 to -18 LUFS; this is
# under it, so the sound is noticed without cutting across what is on screen.
TARGET_LUFS = -21.0
PEAK_CEILING = 10 ** (-3 / 20)


def silence(seconds):
    return [0.0] * int(seconds * RATE)


def strike(buf, at, freq, amp, partials, glide=None):
    """Add one struck note into `buf` starting at `at` seconds.

    `partials` is (ratio, level, decay seconds): each overtone rings for its own time.
    `glide` is (start ratio, seconds): the pitch slides from freq*ratio up to freq.
    """
    start = int(at * RATE)
    attack = int(0.004 * RATE)
    longest = max(d for _, _, d in partials)
    n = min(len(buf) - start, int(longest * 7 * RATE))
    phases = [0.0] * len(partials)
    for i in range(n):
        t = i / RATE
        f = freq
        if glide:
            ratio, secs = glide
            k = min(1.0, t / secs)
            # Ease out, so the slide settles on the note rather than arriving at speed.
            f = freq * (ratio + (1 - ratio) * (1 - (1 - k) ** 3))
        env = 0.5 - 0.5 * math.cos(math.pi * i / attack) if i < attack else 1.0
        s = 0.0
        for j, (r, level, decay) in enumerate(partials):
            phases[j] += 2 * math.pi * f * r / RATE
            s += level * math.exp(-t / decay) * math.sin(phases[j])
        buf[start + i] += amp * env * s


def room(dry, mix, size=1.0):
    """A small Schroeder reverb: four combs in parallel, two allpasses in series."""
    wet = [0.0] * len(dry)
    for delay_ms, feedback in ((29.7, .77), (37.1, .75), (41.1, .73), (43.7, .71)):
        d = int(delay_ms * size * RATE / 1000)
        line = [0.0] * d
        damp, lp = 0.35, 0.0
        for i, x in enumerate(dry):
            y = line[i % d]
            # Damping in the loop: highs die first in a real room, and without it the tail rings.
            lp = y * (1 - damp) + lp * damp
            line[i % d] = x + lp * feedback
            wet[i] += y * 0.25
    for delay_ms in (5.0, 1.7):
        d = int(delay_ms * RATE / 1000)
        line = [0.0] * d
        for i, x in enumerate(wet):
            y = line[i % d]
            v = x + y * 0.5
            line[i % d] = v
            wet[i] = y - v * 0.5
    return [a * (1 - mix) + b * mix for a, b in zip(dry, wet)]


def fade_tail(buf, seconds):
    n = int(seconds * RATE)
    for i in range(n):
        buf[len(buf) - n + i] *= 0.5 + 0.5 * math.cos(math.pi * i / n)
    return buf


# Bell: a light inharmonic partial at 2.76 gives the "struck metal" colour without the clang of a
# real church bell's minor third.
BELL = [(1, 1.0, 0.42), (2, 0.22, 0.20), (2.76, 0.07, 0.09), (4.07, 0.035, 0.05)]
# Glass: brighter and shorter than the bell, so four of them in a row stay distinct.
GLASS = [(1, 1.0, 0.30), (2, 0.14, 0.12), (3, 0.05, 0.07), (5.4, 0.02, 0.035)]


def chime():
    """G5 then D6, 85 ms apart. The fifth is the most consonant interval after the octave, and
    rising reads as "done" rather than "warning"."""
    buf = silence(1.2)
    strike(buf, 0.000, 783.99, 0.70, BELL)
    strike(buf, 0.085, 1174.66, 0.85, BELL)
    return fade_tail(room(buf, 0.18), 0.25)


def pop():
    """One note that slides up a fifth into A5 over 45 ms. Short enough to disappear under a
    game; the slide is what makes it sound like a confirmation instead of a beep."""
    buf = silence(0.4)
    strike(buf, 0.0, 880.0, 1.0, [(1, 1.0, 0.075), (2, 0.18, 0.04), (3, 0.05, 0.025)],
           glide=(0.67, 0.045))
    return fade_tail(room(buf, 0.12, size=0.6), 0.12)


def sparkle():
    """G major, G5 B5 D6 G6, 55 ms apart, in glass. The most celebratory of the three."""
    buf = silence(1.4)
    for k, (f, a) in enumerate(((783.99, .55), (987.77, .58), (1174.66, .62), (1567.98, .70))):
        strike(buf, k * 0.055, f, a, GLASS)
    return fade_tail(room(buf, 0.26), 0.3)


def write_wav(path, samples, gain):
    frames = b''.join(struct.pack('<h', max(-32767, min(32767, round(s * gain * 32767))))
                      for s in samples)
    with wave.open(path, 'wb') as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(frames)


def momentary_max(path):
    """Highest 400 ms loudness in LUFS. Padded with silence so a sound shorter than the window
    is still measured over a whole one."""
    log = subprocess.run(
        [FFMPEG, '-hide_banner', '-nostats', '-v', 'verbose', '-i', path,
         '-af', 'apad=pad_dur=1,ebur128=framelog=verbose', '-f', 'null', '-'],
        capture_output=True, text=True).stderr
    values = [float(m) for m in re.findall(r'\bM:\s*(-?[\d.]+)', log)]
    return max(values)


def main():
    os.makedirs(OUT, exist_ok=True)
    for name, make in (('chime', chime), ('pop', pop), ('sparkle', sparkle)):
        samples = make()
        path = os.path.join(OUT, name + '.wav')
        gain = PEAK_CEILING / max(abs(s) for s in samples)
        write_wav(path, samples, gain)
        # Loudness scales linearly with gain in dB, so one measurement is enough to land on it.
        gain *= 10 ** ((TARGET_LUFS - momentary_max(path)) / 20)
        peak = max(abs(s) for s in samples) * gain
        if peak > PEAK_CEILING:
            gain *= PEAK_CEILING / peak
        write_wav(path, samples, gain)
        print(f'{name:8} {len(samples) / RATE:.2f}s  {momentary_max(path):6.1f} LUFS  '
              f'peak {20 * math.log10(max(abs(s) for s in samples) * gain):5.1f} dBFS')


if __name__ == '__main__':
    main()
