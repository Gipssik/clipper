# clipper-capture — instant replay daemon

A background recorder that keeps the last N seconds of a monitor encoded in a ring buffer and
writes them out as an MP4 when a hotkey is pressed. Replaces the dependency on NVIDIA Instant
Replay. Written in Rust, shipped as a single exe, launched and configured by the Electron app.

The design goal that decides every trade-off below: **cost the game as little frame time as
possible.** That is why nothing in the hot path ever touches system RAM, and why the ring buffer
holds compressed bytes rather than pictures.

## The load-bearing constraint

You cannot ring-buffer raw frames. 1080p60 BGRA is 500 MB/s; 4K60 is 2 GB/s. A one-minute buffer
would be 30–120 GB. So the buffer holds **already-encoded** video, and everything else follows:
the encoder runs continuously whether or not you ever press the hotkey, and the buffer can only be
cut at a keyframe.

## Pipeline

```
Windows.Graphics.Capture ──► D3D11 texture (FP16 scRGB or BGRA, stays on the GPU)
                                  │
                                  ▼  one compute dispatch: scale + tone map + NV12 pack
                             NV12 texture
                                  │
                                  ▼  Media Foundation hardware MFT, fed the texture directly
                             H.264 Annex B, IDR every 2 s, no B-frames
                                  │
   WASAPI loopback ──► AAC (ADTS) ─┤
                                  ▼
                        TS muxer ──► ~2 s .ts segments in %LOCALAPPDATA%\clipper\buffer
                                  │   (oldest evicted once the window is full)
                                  │
              [Ctrl+Alt+F12] ─► pin segments ─► byte-concat ─► ffmpeg -c copy ─► clip.mp4
                                  │
                    [Alt+F9] ─► the same packets, teed ─► Recordings\rec_….ts ─► -c copy ─► rec_….mp4
```

The saved clip lands in the clips folder the app already scans, so `folder:watch` and `syncGrid()`
make it appear in the grid with no new UI plumbing.

## Decisions, and what they cost

**Windows.Graphics.Capture, not Desktop Duplication.** WGC survives fullscreen-exclusive games,
display mode changes and HDR without needing re-acquire logic, and on Win11 the capture border can
be turned off (`IsBorderRequired = false`). DDA's only remaining advantage is pre-1803 support,
which we do not need. Frames arrive as D3D11 textures on a free-threaded frame pool.

**Fixed-rate pull, constant frame rate out.** WGC delivers on change, not on a clock. A
high-resolution waitable timer ticks at the target fps and encodes whatever the latest texture is,
repeating the previous frame when the game is running below target. CFR keeps the muxer and any
later trim in Clipper trivial; a repeated frame costs almost nothing in the bitstream.

**Repeated frames are copied, not converted.** On a tick with no new frame the scale / tone map /
NV12 pass used to run again over the same source; now `Converter::repeat` copies the last finished
NV12 into the next surface instead. Measured on a still screen at `low` quality, RTX 5080, 1440p240,
two alternating runs each:

| Repeated frame                    | Recorder 3D | Encode | GPU board power |
| --------------------------------- | ----------- | ------ | --------------- |
| converted again (v3.2.0)          | 1.46%       | 5.0%   | 24.6 W          |
| **copy of the last NV12**         | **0.74%**   | 5.0%   | **22.9 W**      |
| same surface handed over again    | 0.00%       | 16.1%  | 20.7 W          |

The last row is not a typo and is not used. Resubmitting the surface the encoder was just given
triples the encode engine's reported busy time, and the clock drop (775 → 645 MHz) explains a
fraction of that; the cause is inside the driver. It draws the least power, but Task Manager is where
people judge a recorder, and it would show one three times busier. With motion on screen nearly every
tick has a new frame, so neither saves anything there — measured equal within noise.

**Frames are copied out of the capture pool, and sampling it directly is slower.** It looks like a
free win to skip `Capture::pump`'s `CopyResource` and let the converter read the pool's texture,
holding the frame open until a fence says the read is done. It was built, and it came out *worse*
with motion on screen: recorder 3D 1.12% → 1.49% and DWM 3.6% → 7.4% with a small animated window,
2.68% → 3.25% scrolling full screen. The likely reason is that pool textures are shared surfaces and
lose the GPU's framebuffer compression, and the Catmull-Rom downscale reads every source texel
several times; the copy pays for the shared surface once. Do not rebuild it without measuring this.

**Read GPU percentages together with the clock.** Task Manager's figure is the share of each second
an engine was busy *at its current clock*. An idle RTX 5080 sits in P8 with memory at 405 MHz of
15001; the recorder's work there reads as 2–3% 3D and 8–10% encode, and the moment enough is moving
for the driver to go to P3 the same work reads 1.1% and 4.9%. This is why the recorder's figure
rises and falls with DWM's under light desktop activity, and why it drops while NVIDIA's own
Instant Replay runs — which holds the card in P5. Compare builds on a fixed scene (`motion.js`-style
scrolling window) and read board power next to the percentages.

**Media Foundation, not NVENC directly.** One code path picks up NVENC, AMD AMF or Intel QuickSync
via `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)`
filtered on `MFVideoFormat_H264`. The MFT is driven async (`MF_TRANSFORM_ASYNC_UNLOCK`, then pump
`METransformNeedInput` / `METransformHaveOutput`), gets the D3D11 device through
`MFT_MESSAGE_SET_D3D_MANAGER`, and is fed textures as `MFCreateDXGISurfaceBuffer` samples — so
there is still no CPU copy. Rate control through `ICodecAPI` — and the settings go on **before** `SetOutputType`, which is the
difference between them working and being politely ignored (see milestone 7). The cost versus
talking to NVENC directly is coarser rate control and no lookahead; the gain is that it works on any
GPU. A direct NVENC backend can be added later behind the same `Encoder` trait.

**Peak-constrained VBR, mid preset, no low-latency mode.** The rule is: spend bitrate, which costs
disk, before spending the encode chip, which costs the machine the game is running on.

* `AVLowLatencyMode` exists for streaming, where a late frame is worse than an ugly one. We write to
  a ring buffer on disk and nothing is waiting on the output, so it is off. Measured free either
  way — 22.5% of the encode engine against 22.6% — so it is off for what it means, not what it costs.
* `AVEncCommonQualityVsSpeed` at **50**, which is the middle of three preset bands rather than the
  top one. This is the most expensive setting in the pipeline and it was set to 80 on a guess that
  measuring disproved; `config::QUALITY_VS_SPEED` carries the table.
* Two bitrates rather than one, and the peak kept close to the mean. The peak is not a luxury, it
  is the only guarantee anybody has about file size: a game that is hard *continuously* — a football
  match, say — spends the whole allowance from start to finish, so at 1.6× the mean a real clip came
  out 60% larger than the number that was chosen. At about 1.35× the promise and the worst case are
  close enough to be the same thing. Rate control here clamps but does not pad, so a single number only
  ever acts as a ceiling: a menu screen costs a fraction of it either way, and the headroom is what
  keeps a firefight from turning to mush. The ring is bounded by duration, not bytes, so the spare
  headroom cannot make it outgrow its window.

The stream is also tagged **limited-range BT.709** on the output media type. An untagged stream is
only assumed bt709 by convention, and the one case where that assumption goes wrong is the case this
whole pipeline exists for — a clip taken off an HDR display.

**Capabilities are probed by doing.** Same lesson as `detectEncoders()` in `main.js`: enumeration
only proves an MFT is registered, not that this machine can use it. At startup we run a throwaway
encode of a few frames and walk down the candidate list on failure, ending at the software MFT.

**No B-frames** (`AVEncMPVDefaultBPictureCount = 0`). Keeps DTS equal to PTS, which keeps the muxer
simple and the pipeline low-latency. Costs roughly 5% bitrate efficiency, which is worth it.

**IDR every 2 seconds.** You can only cut the ring at a keyframe, so the GOP length *is* the trim
accuracy at the head of a saved clip — the clip may start up to two seconds earlier than asked,
which against a buffer measured in minutes is not a cost. What it buys is real: an I-frame runs
about ten times a P-frame, so at 60 fps a one-second GOP spends roughly a sixth of the stream on
keyframes, and halving their number gives those bits back to the other 59 frames.

The spacing is set with `MF_MT_MAX_KEYFRAME_SPACING` on the output media type, not with
`CODECAPI_AVEncMPVGOPSize`, and both have to be set before the output type is applied.

**A real downscale filter.** The first version let the sampler do the scaling: read the source
through a linear sampler at the destination's resolution and the downscale comes free in the fetch
the tone map needs anyway. That is exactly a box filter at 2x and under-filters at every other
ratio. It is now a Catmull-Rom kernel stretched to the destination pixel pitch and evaluated at
every source texel inside its support — up to 8 taps per axis, skipped entirely when there is no
scaling to do. Measured on a zone plate at 1440p→720p: **+2.9 dB** against a proper Lanczos
reference, for **0.6 points of the 3D engine** (0.79% → 1.42%).

The tone map runs once per *output* pixel rather than once per tap. With a 64-tap kernel that is the
difference between one `pow` per pixel and sixty-four, and it is why an HDR capture costs the same
as an SDR one. The filter then works in linear light on HDR, which is the more defensible of the two
anyway; on SDR the source is already sRGB-encoded, so it is the gamma-space filtering every other
video scaler does.

**Tone mapping in the shader.** On an HDR display, WGC gives FP16 scRGB; encoding that as 8-bit
without tone mapping is what produces the washed-out grey captures people complain about.

scRGB is *already* linear with bt709 primaries — 1.0 is fixed at 80 nits, and wide-gamut colours
are carried as values outside [0,1] rather than by a different matrix. So there is **no
bt2020→bt709 conversion in this path**, unlike the zscale chain `runEncode()` uses on files that
really are tagged bt2020/PQ. What there is instead is a divide by the display's SDR white level:
the desktop's white on an HDR display is whatever the SDR brightness slider says — 200 nits on this
machine, so 2.5x scRGB — and skipping that divide blows the capture out by exactly that factor.
`display.rs` reads the real number rather than guessing.

The shader then clamps the negatives scRGB uses for out-of-gamut colour, applies **mobius** to the
brightest channel and scales the triplet by the same ratio (a per-channel curve would pull bright
saturated colours toward white), encodes the bt709 transfer, and writes Y and UV straight into the
encoder's NV12 texture. The output must be **tagged bt709 explicitly** or players will still
believe it is HDR. With HDR off, peak is 1.0 and the curve is skipped entirely — otherwise it would
only darken midtones of content that never exceeds white.

**Our own MPEG-TS muxer.** The alternative was keeping a bundled `ffmpeg.exe` resident and feeding
it Annex B and ADTS over named pipes, which would have been zero muxing code but would have derived
A/V sync from "both streams started at about the same time" rather than from real timestamps. Ours
is roughly 300 lines, drops the resident process, and gives exact QPC-derived timestamps. Owning it
also means we prepend the stored SPS/PPS to every IDR ourselves, which sidesteps per-vendor
differences in whether an MFT repeats its sequence header.

Format details: PAT/PMT at the head of every segment (so each is independently playable and plain
byte concatenation works, as in HLS), PID 0x100 video (stream type 0x1B), PID 0x101 audio
(0x0F, ADTS AAC), PCR on the video PID, one monotonic 90 kHz clock derived from QPC for the whole
session so concatenated segments stay continuous.

**Disk ring, not RAM ring.** One minute at 30 Mbps is 225 MB; ten minutes is 2.2 GB. In RAM that is
a real cost on a gaming machine; on an SSD it is 3.75 MB/s of sequential writes, which is noise.
It also survives a daemon crash, and it makes long buffers cheap. Default location
`%LOCALAPPDATA%\clipper\buffer`, configurable under Advanced for anyone who wants the writes off
their system drive. Segments start only on an IDR and target ~2 s. Eviction drops whole segments
from the front once total duration exceeds `bufferSeconds` plus two segments of slack, so a save
taken right at the boundary still has the full window.

**Save is a pin, then a copy.** On hotkey: snapshot the manifest and mark those segments pinned so
eviction cannot delete them mid-copy, then on a background thread concatenate their bytes and run
the bundled ffmpeg — `-c copy -bsf:a aac_adtstoasc -movflags +faststart`. Capture and encode never
block on any of this, so the game does not stutter while a clip is written.

**Audio.** WASAPI loopback on the default render endpoint, polled rather than event-driven —
loopback does not support event callbacks. Loopback delivers nothing while no audio is playing, so
we also open a silent render stream to keep the engine pumping, and fill any real residual gap from
the device's own QPC timestamps — otherwise the audio track ends up shorter than the video by
exactly the length of every silence. *Real* is load-bearing: see what the desktop audio corrected. The
endpoint's mix format is whatever the user's device says (often 48 kHz float32 stereo, but 44.1,
96 kHz and 7.1 all happen in the wild), and the AAC MFT wants 16-bit PCM stereo at 44.1 or 48 kHz,
so there is a convert/downmix/resample step — use `CLSID_CResamplerMediaObject` rather than writing
one. The leg follows the default endpoint when it changes (headphones plugged in, a headset picked
in the volume flyout) and reopens without disturbing video — polled, not an `IMMNotificationClient`;
see what switching outputs corrected, which is also where this sentence stopped being a plan.

**The microphone, mixed into the same track.** A second WASAPI stream, this time a capture
endpoint, summed with the desktop leg into one stereo track. On by default, because nobody thinks to
turn the microphone on *before* the round worth keeping, and a clip of the match you were calling
out in with every voice missing is a clip of the wrong thing.

*One track rather than two.* MPEG-TS and MP4 would both carry a second audio stream happily, and it
would be the better answer for editing. It is the worse answer for everything this tool is for: a
clip goes to Discord or a group chat, and most of what plays it there plays the first track and
silently ignores the rest — so "separate tracks" means people posting clips with no voice in them
and no way to tell.

Three things make the mix work, and each of them is a thing that goes wrong if skipped:

* **One rate, and the audio engine does the conversion.** Mixing means agreeing on a sample rate,
  and a headset at 44.1 kHz beside a render endpoint at 48 does not.
  `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` hands the engine the format we already work in rather than
  putting a resampler in the recorder; when the mic is natively at our rate, which is the common
  case, it costs nothing.
* **Aligned by timestamp on every poll, not by sample count.** Both streams are gap-filled onto
  their own device's timeline first, then the mix takes from each the span it is emitting. Assuming
  the two stay in lockstep would work for a minute and slide apart over an hour, because they are
  two independent clocks.
* **The mix waits for the slower source, but not forever.** Both are polled on the same tick and one
  routinely has a few milliseconds the other does not; waiting is what keeps a voice lined up with
  the game. A mic that has stopped delivering — unplugged, asleep, taken by an exclusive-mode
  application — must not take the audio track down with it, so past 200 ms of lag the mixer fills
  its share with silence and carries on, and a mic that fails outright is retried every three
  seconds while everything else keeps recording.

*Following the default is a standing promise, not a choice made at open.* With `micDevice` empty,
the mixer asks Windows for its default capture endpoint once a second — `GetDefaultAudioEndpoint`
on a cached enumerator, a lookup inside the process — and when the id no longer matches the open
mic, drops it and lets the retry open the new one on the same poll. It is the unplug-and-replug path
exactly, so it costs the same few milliseconds of splice in the voice and leaves the buffer, the
video and the desktop leg alone. Before this the mic was resolved only when the pipeline was built,
so a default changed mid-session kept recording the old device until something rebuilt it. Polled
rather than an `IMMNotificationClient` for the same reason the loopback is: nothing to register, no
callback thread, and a quarter of a second is not a delay anybody switching mics will notice.

Summed at unity, not each halved: halving would quietly make every clip's game audio 6 dB softer
than it was before microphones existed, including through the stretches where nobody says anything.
What unity costs is headroom, and **a limiter rather than a clamp** is what pays it. A microphone set
hot in Windows on top of loud game audio does reach full scale, and clamping there puts a rasp on
exactly the thing anybody will notice — their own voice. One block of lookahead (256 frames, 5.3 ms)
means the gain a block needs is reached by the end of the block before it, so a transient can never
arrive ahead of the reduction meant for it; recovery is a quarter-second, slow enough not to pump.
Below the ceiling it does nothing at all, and `cargo test` holds it to that: with headroom every
sample comes out bit-identical.

**Noise suppression is RNNoise, on the mic leg only, before the boost.** `denoise.rs` wraps
`nnnoiseless`, the pure-Rust port, so there is no C toolchain and no DLL. It sits between the mic
endpoint and its track, so the boost lifts a voice that has already been cleaned rather than the
room it came from, and the desktop leg never passes through it.

**One strength slider, two stages, because the network alone is not enough.** The first version
was only a dry/wet blend, and it could only ever make the network *weaker*: 100% was plain RNNoise,
and in use that meant nothing below 90% did much and 100% still let noise through. Measured, the
network takes pink noise down ~50 dB but flat hiss by 1.5 dB and keyboard clicks by little more,
because it hears both as consonants. So the slider is now:

* **0–40%, a blend, which is a floor.** Where the network silences a frame, what is left is the dry
  signal at `1 - wet`, and the floor in dB is the slider's own number. The dry signal covers the
  network's mistakes on breaths and word tails, which is what keeps the gentle end natural.
* **40–100%, the full network plus a gate on its voice detector.** The detector does better than
  the gains, but not by as much as a short probe suggests: over a long run it scores hiss up to
  0.83 and clicks up to 0.58, against 0.97 and up through a word. Each step closes the gate deeper,
  `strength - 40` dB down to -60, and raises the score it opens at from 0.5 to 0.9 on a
  square-root curve — the fooling noises sit in the 0.7s and 0.8s, and on a straight line hiss went
  from -11 dB at 80% to -69 dB at 100%, which is a switch rather than a slider. It looks two frames
  ahead so it opens before the first consonant, holds 200 ms after a frame that was certainly voice
  so it does not clip word tails, and closes at 6 dB per frame so a pause fades rather than snaps.
  The hold is keyed on a clear decision, not the highest score in the window: over 200 ms of hiss
  the highest score is routinely high enough to hold a gate open for ever.

`cargo test` holds it to a table. Noise cut / voice cut in dB, two seconds of each noise then voice
over it:

| | 20% | 40% | 60% | 80% | 100% |
|---|---|---|---|---|---|
| pink | -20 / -1.0 | -47 / -1.1 | -66 / -1.1 | -122 / -1.1 | -122 / -1.1 |
| hiss | -9 / -0.3 | -10 / -0.3 | -10 / -0.3 | -23 / -0.3 | -69 / -0.3 |
| clicks | -7 / 0.0 | -8 / 0.0 | -28 / 0.0 | -48 / 0.0 | -68 / 0.0 |

and the first 50 ms of a word out of hiss comes through at -1.9 dB with the gate at its hardest. The
fixtures are synthetic; a real voice scoring lower than the synthetic one is the risk at the top
of the slider, and the "hear yourself" test is how anybody finds out.

The network runs a frame (480 samples, 10 ms) behind its input, and that is **taken back out, not
stamped around**. The warm-up frame is discarded, so output sample `n` is input sample `n` and
carries the endpoint's timestamps unchanged; `cargo test` finds the correlation peak at lag zero to
hold it there. Keeping the warm-up and stamping the stream a frame early is equally exact on paper,
and was the first version — but switching suppression on mid-session then handed the mix 10 ms of
audio for a stretch it had already emitted, and every toggle showed up as 479 dropped frames. What
the latency and the gate's lookahead do still cost is availability: the mix waits up to four
frames longer for the mic, which moves the live `audioOffsetMs` by a few tens of milliseconds and
no clip at all. The lookahead runs at every strength, so crossing 40% never changes how far behind
the output runs.

It runs at 48 kHz only, which is what the network was trained at. A mix running at 44.1 kHz reports
`noiseError` instead of feeding it audio at the wrong pitch. Switching it on or off restarts the
mic track (one splice of a few milliseconds in the voice) and keeps the ring; strength is a blend
weight and a gate setting and applies in place. Measured cost: 60 s of audio in 0.17 s, 0.28% of
one core; the gate is a few comparisons per 10 ms and does not show.

**`mictest` is the mixer, not a copy of it.** The panel's "hear yourself" button spawns
`clipper-capture mictest`, which runs `audio::Mixer` with the desktop leg off and writes the result
to a WAV — the same suppressor, boost and limiter in the same order, so what plays back is what a
clip will contain. It is a process of its own rather than a daemon command so the test never
disturbs the ring, and it shares the microphone with a running daemon, as shared-mode WASAPI
allows. It stops on a line on stdin or at end of input, so an app that dies mid-test cannot leave
it listening, and prints a level line ten times a second for the panel's meter.

**Neither device's clock is our clock.** A USB microphone free-runs, and its packet timestamps
jitter either side of where a sample count says they should be; so, on some machines, do a render
endpoint's. Silence is right for an engine that stopped and badly wrong for a device that is merely
keeping its own time — see what the microphone corrected, below. Both legs are therefore rate
matched by the same `Pacer`: the error between the device's timeline and ours drives a first-order
loop that resamples by a fraction of a percent, and silence is kept for gaps past 50 ms, or for a
packet flagged `DATA_DISCONTINUITY`, where it is a real hole. They differ in one number. The mic has
no deadband. The desktop leg has 10 ms, inside which the ratio is exactly one and the samples pass
through bit for bit — which is where a render endpoint that keeps time always sits.

**No converter in the path when there is nothing to convert.** The mic is opened at the endpoint's
own format whenever its rate already matches the mix, which is almost always — `AUTOCONVERTPCM` has
to be paired with `SRC_DEFAULT_QUALITY`, and a resampler with nothing to resample can only cost
quality. The conversion flags are for the genuinely mismatched device.

**A mono endpoint goes to both ears.** Plenty of headset microphones report one channel, and the
stereo weights would have put such a voice entirely in the left one.

**A/V sync** comes from stamping video and audio against one QPC clock at the moment of capture,
never from arrival order.

**Record mode.** `game` or `always`, user's choice. A foreground-window watcher polls
`GetForegroundWindow` about twice a second (cheap) and resolves the process. In `always` mode the
watcher still runs, but only to name the output subfolder.

**"Is this a game" is asked in layers.** Covering the screen is not an answer — a maximised terminal
covers the screen, and so does a chat window someone pressed F11 in. Most certain first:

1. **The user's own lists.** An explicit exclude beats an explicit include beats everything below.
2. **A built-in list of things that are never games** — the shell, browsers, chat apps, terminals,
   editors, launchers, media players. These are exactly the programs people run full-screen, which
   is why the fullscreen rule alone got all of them wrong.
3. **Windows already knows.** `HKCU\System\GameConfigStore\Children` is where Game Bar records
   every executable it has identified as a game, by full path. On the machine this was written on it
   held two entries, both of them games, and nothing else.
4. **What the process is doing to the GPU.** `gpuload.rs` reads
   `\GPU Engine(pid_*_engtype_3D)\Utilization Percentage` through PDH — the same counter Task
   Manager's GPU column shows. Ten percent sustained across three polls promotes a full-screen
   window to a game. The load is summed over the foreground process's **whole tree**, not just the
   process owning the window: anything built on Chromium renders in a separate GPU child process,
   and a full-screen WebGL window measured at exactly 0% until the children were counted.

The threshold and the "sustained" part are load-bearing in opposite directions. High enough that one
scroll of a heavy page cannot reach it, a threshold also excludes games vsync-capped on a fast card;
low enough to catch those, one animation frame in a desktop app trips it. Requiring the load to
*stay* up separates them: no window animation lasts a second and a half, and no game fails to.

The verdict then **latches for as long as that window stays in front**, because a game sitting on a
pause menu must not fall under the line and stop the recorder — precisely the moment somebody is
about to want the last minute back.

**Two questions, two standards of proof.** `is_game` decides the folder and wants to be right;
`worth_recording` decides whether the recorder runs at all in `game` mode and is deliberately
generous — anything full-screen that is not on the never-a-game list keeps it going. A clip filed in
the wrong folder can be moved; a clip that was never recorded cannot. Collapsing the two into one
flag means every point you move the GPU threshold trades lost footage against unwanted folders, and
there is no setting of it that is right.

Whatever it decides, it says why, and the settings panel shows it with one-click "treat as a game" /
"never a game" buttons that write the two lists. A classifier that is occasionally wrong and always
legible beats one that is silently wrong.

**Per-game subfolders.** The daemon knows the foreground process at save time, so a clip taken in a
game goes to `<outputPath>\<GameName>\clip_2026-09-21_18-42-03.mp4`. That feeds the
subfolder-as-category model `folder:scan` already implements, for free. **Only a game gets a folder
of its own** — everything else shares one `Desktop` folder, because the point of the setting is a
library sorted by game and a folder called `WindowsTerminal` holding one clip is not that.

**And the name is not the executable's.** Unreal ships a binary named after the internal project,
and the two are routinely unrelated: REMATCH runs `RuntimeClient-Win64-Shipping.exe`, MECCHA
CHAMELEON runs `PenguinHotel-Win64-Shipping.exe`, Half-Life: Alyx runs `hlvr.exe`. `gamename.rs`
takes the name from wherever somebody already wrote it down, most authoritative first: Steam's
`appmanifest_*.acf` (which holds the store name beside the install directory it belongs to), then
a launcher's install folder, then two directories above Unreal's `Binaries`, then the executable's
version resource, then the file name. `clipper-capture names` prints the answer for every game Game
Bar has on file, which is how the layers are checked against a real library rather than a guess.

The window title is deliberately not one of the layers. It is often the best-looking string
available and it is the only one that changes while you play — a title reading `Game` in the menu
and `Game - Level 3` in play would file one session into two folders, and a folder that splits is
worse than a folder that is ugly.

**The hotkey lives in the daemon**, not in Electron's `globalShortcut` — the daemon is the thing
that is always running. Default `Ctrl+Alt+F12`, configurable. Each hotkey is registered only while
its feature is on, and both arrive on the record loop's one message queue, so `hotkey::fired()` drains
them together — a `PM_REMOVE` peek for one id would throw the other's message away.

**Alternative binds can be a controller button.** Each hotkey has an `altHotkey` beside it — a
second key combination, registered as ids 3 and 4, or a button on a game controller written as
`MOZA R5 Base / Button 12 [346E:0004]`. The primary binds stay keys, so one always works with
nothing plugged in. `gamepad.rs` reads controllers through **Raw Input with `RIDEV_INPUTSINK`**, on a
message-only window in a thread of its own: `Windows.Gaming.Input` would be less code and stops
reporting while our process is in the background, which is always. Reports are decoded with
`HidP_GetUsages` (button page) and `HidP_GetUsageValue` (hat switch) against the device's own
preparsed data, so nothing is specific to any vendor. A bind matches on vendor and product id plus
the control's name — that survives replugging, where a device path does not.

Three things came out of measuring it against a real wheel base (MOZA R5, 128 buttons and a hat):

* **It reports ~900 times a second while nobody touches it.** Waking per `WM_INPUT` and parsing into
  strings cost **2.1% of a core**. The reader now sleeps 16 ms and drains the queue, and compares
  reports as a 256-bit mask so text is only built on a change: **0.31%**. It also runs only while a
  bind names a controller or the panel is asking which button to use.
* **A button can be held forever.** The R5 reports one of its buttons as permanently down, which
  read as a press the moment the reader started. The first report from each device is a baseline,
  not a press.
* **Choosing a button goes through the hotkey capture.** `listen` with `pads: true` opens the reader
  and offers the first button to the same first-writer-wins slot the keyboard hook writes, so a key
  or a button, whichever comes first, is the answer; while it is open, a button is never an action.

`clipper-capture pads` lists the controllers and `pads --watch <s>` prints every press and the
report rate — exactly what a bind can be set to, and what the reader costs.

**Recording on demand is a second reader of the same stream, not a second recorder.** A second
hotkey (`Alt+F9`, ShadowPlay's own) records from one press to the next. `record.rs` takes a copy of
every packet on its way to the ring, so a recording costs no encode silicon, is at the replay's
quality, tone map and audio mix by construction, and leaves the ring untouched — the replay hotkey
works in the middle of a recording because nothing about the replay has changed. Running a second
encoder was the alternative and would have doubled the one cost this whole design exists to keep
down.

What the recording does *not* share is the replay's idea of when and where. It records the screen
for as long as it was asked to, whatever is in front: the pipeline's condition to exist is
`replay_wants || record_wanted`, and game detection only feeds the first. And it goes to
`<outputPath>\Recordings\`, not a folder per game. With replay off the ring is simply absent —
`Pipeline::ring` is an `Option` — and the pipeline exists only while a recording does. Switching
replay on or off under a running recording adds or drops the ring without rebuilding anything.

* **It starts on the keypress, not on the next keyframe.** A recording can only open on an IDR, and
  the stream has one every two seconds. `Encoder::force_keyframe` (`CODECAPI_AVEncVideoForceKeyFrame`)
  asks for one on the next frame. Measured with the replay already running: **6.03 s recorded for a
  6 s request**, where waiting would have lost up to two. With replay off the pipeline has to be
  built first, and that costs about **0.8 s** at the head.
* **Written as TS, published as MP4.** While it runs it is `rec_<time>.ts`, a stream valid up to its
  last byte, with PAT/PMT repeated at every keyframe so it opens from anywhere. Stopping remuxes it
  (`-c copy`) to `.mp4.part` and renames that into place, so the library never lists a half-written
  MP4; Clipper's scan does not list `.ts` at all, and the folder watcher ignores `.ts`/`.part` changes
  so a growing recording does not hold the rescan's debounce off. `record::recover` finishes any
  `rec_*.ts` a previous run left, at startup. Measured: a recorder killed with `SIGKILL` nine seconds
  in left a 1.1 MB `.ts`, and the next start published a clean 6.6 s MP4 from it.
* **It survives a rebuild in one file.** A rebuild *detaches* the recording; the next pipeline's
  first keyframe is placed one frame after the last one written, so the file closes the gap rather
  than holding a frozen frame across it. Only a change the file cannot carry — a new resolution, audio
  appearing or disappearing, compared as `record::Format` — ends it and starts `…_2.mp4`. Measured
  through a watchdog rebuild: 941 frames over 15.7 s at 60 fps with no gap in the video, and one
  18 ms splice in the audio where the new pipeline's audio begins just after its keyframe.
* **A recording that cannot write stops, and keeps what it has.** A write error — a full disk — is
  kept on the recording rather than propagated, because it is no reason for the pipeline feeding the
  replay to fail; the loop sees it, publishes what was written and says why.

## Configuration

`%APPDATA%\clipper\capture.json` — a sibling of `prefs.json`, not a section inside it. `savePrefs()`
does read-modify-write of the whole prefs file from the renderer's settings object; a daemon reading
that same file races the write, and a daemon that ever wrote to it would clobber UI state. Separate
file, written atomically by Electron (temp + rename), schema owned by the daemon.

```jsonc
{
  "version": 1,
  "enabled": true,
  "recordMode": "game",            // "game" | "always"
  "bufferSeconds": 60,             // min 30, max 600
  "monitor": { "device": "\\\\.\\DISPLAY1", "friendly": "LG HDR 4K" },
  "quality": "high",               // tier -> { maxHeight, fps, bitrate, codec }
  "outputPath": "D:\\Clips",
  "perGameSubfolder": true,
  "gameDetection": "auto",         // "auto" = the classifier; "fullscreen" = the old rule
  "notifyOnSave": true,            // a Windows toast when a clip lands; read by Electron
  "hotkey": "Ctrl+Alt+F12",
  "altHotkey": "",                 // a key combination or "Name / Button 12 [VID:PID]"; "" for none
  "record": {                      // recording on demand; either switch keeps the daemon running
    "enabled": false,
    "hotkey": "Alt+F9",
    "altHotkey": ""
  },
  "audio": {
    "desktop": true,
    "mic": true,                   // mixed into the same track
    "micDevice": "",               // "" = follow the system default; otherwise an endpoint id
    "micGainDb": 0,                // boost before the mix; applied in place, no rebuild
    "noiseSuppression": false,     // RNNoise on the mic; switching restarts the mic track only
    "noiseStrength": 70            // 0-40 noise floor in dB, 40-100 adds a voice gate; in place
  },
  "toneMap": "auto",               // "auto" | "always" | "off"
  "segmentDir": null,              // null = %LOCALAPPDATA%\clipper\buffer
  "includeProcesses": [],
  "excludeProcesses": []
}
```

Monitor identity is stored as both the GDI device name and the friendly name, matched on friendly
name first — device names shuffle when displays are replugged.

Quality is a tier that expands to resolution / fps / bitrate rather than four separate sliders, and
every control in the UI gets its one-line cost tip, as the rest of the app does:
*"1440p — sharper text and distant detail, about 1.8× the file size and a little more encode time."*

## Control channel

`\\.\pipe\clipper-capture`, newline-delimited JSON. A restart would be simpler but drops the buffer,
so nudging a slider would cost you the last minute of footage.

| Command | Effect |
|---|---|
| `reload` | Re-read config without dropping the buffer — bitrate, duration, path, hotkey, record mode |
| `status` | Recording? fps, buffer seconds held, disk used, last error |
| `save` | Same as the hotkey, so the UI can offer a button |
| `record` | Start or stop a recording: `{"on": true}` starts, `false` stops, no `on` toggles as the hotkey does |
| `quit` | Flush and exit |

`status` carries the counters that make a stall diagnosable in one look rather than by archaeology:
`framesEncoded`, `fps`, `framesDropped`, `missedTicks`, `captureFramesIn`, `poolRebuilds`,
`segmentsWritten`, `sinceProgressMs`, `audioStartMs`, `audioOffsetMs`.

The microphone reports as three separate facts — `micWanted`, `micActive`, `micDevice`, plus
`micError` — because "there is no voice in my clip" has three different causes and the settings
panel has to be able to tell them apart: not asked for, asked for and running, asked for and
refused. `clipper-capture inputs` lists the endpoints the picker offers, with the rate each runs at.

"The voice sounds rough" needs its own answers, and they are also in `status`: `wouldClipFrames` and
`minGain` say whether the two sources together are actually reaching full scale, `micPadded` and
`micDropped` say whether the mix ever had to invent or discard samples, and `micDropouts` says
whether the microphone fell far enough behind to be left out of a round. `clipper-capture audio
--mic --wav out.wav` writes the mix **and each leg beside it**, because a fault in a sum cannot be
attributed to a source by looking at the sum.

Pushed events: `clip-saved` (with the path and the game), `error`, `state` (whether the replay is
buffering), `foreground` (what is in front and what the classifier made of it), `display` (whether
the configured monitor is there), `rebuilding` (with the reason), `reloaded`, and for recordings
`record-started`, `record-stopped` (the file is being finished) and `record-saved` (it is in place).
When a recording rolls over into a new file on a format change, that `record-stopped` carries
`continuing: true` and the next `record-started` carries `continued: true`, so the app neither shows
the recording as stopped nor announces a start nobody asked for.

`status` says `recording` for the replay buffer and `capturing` for the pipeline, which differ while
a recording runs with replay off, plus a `record` object: `active` (asked for), `waiting` (asked
for, no frame written yet — the display is missing or the encoder is still starting), `elapsedMs`,
`durationMs`, `bytes`, `path`. `recordHotkeyOk` sits beside `hotkeyOk`. A `reload` is answered
with a fresh `status` as well, so a control that changed something redraws immediately rather than
looking stuck until the next poll.

When a clip lands the UI shows its own toast *and* Electron raises a silent Windows notification —
the hotkey is pressed while you are looking at a game, where Clipper's own toast is somewhere
behind it.

**Nothing needs the process restarted.** Monitor, quality, tone mapping, audio and the segment
directory all rebuild the pipeline in place, which is `Config::needs_restart`; everything else is
applied without even that. Killing and respawning the daemon for a setting is what made settings
look like they were not applying — see milestone 7.

## Lifecycle

The daemon starts with the app and stops with it, whenever either instant replay or recording on
demand is on. The app gains a tray icon and keeps running when
its window is closed, so the buffer survives closing the window.

- Electron spawns the daemon on `app.whenReady`, passing `--parent-pid <pid>`. The daemon opens a
  handle to that process and exits when it dies, so a crashed or killed Electron never orphans a
  recorder. (A Win32 Job Object would be the other way, but that needs a native module; watching
  the parent PID is pure Rust.)
- `window-all-closed` hides to tray instead of quitting when capture is enabled. Tray menu: show
  window, capture on/off, save clip now, quit.
- `app.requestSingleInstanceLock()` on the Electron side and a named mutex on the daemon side —
  two Clippers must not both be recording.
- The daemon logs to `%LOCALAPPDATA%\clipper\capture.log`, rotated and small. It is a background
  process with no console; without a log, diagnosing it is guesswork.
- Stale segments from a previous run are swept at startup.
- **A pipeline that stops producing is rebuilt.** Every stage can stop without failing: `pump`
  returns "nothing changed" when the capture is dead, which is what it also returns on a still
  screen, and `submit` drops a frame and returns Ok when the encoder will not take input. So
  `tick()` can go on succeeding forever while nothing reaches the ring — and the recorder reports
  itself as recording, at a plausible frame rate, holding a buffer that quietly stopped advancing.
  A second-by-second check on the packet count turns that into a rebuild, a log line naming every
  counter, and a `rebuilding` event. Five seconds is the bar, which is far past anything a busy
  moment can produce; `--stall-after` makes it happen on demand rather than leaving it a claim.
- The heartbeat is a minute, not five. A session somebody gave up on after four left no trace.
- The pipeline is disposable and the daemon is not. A display switched to HDR, a monitor turned off
  to spare an OLED, a resolution change, a driver hiccup — each is a rebuild, not a reason to exit.
  `tick()` errors are counted rather than propagated for exactly this reason.

## Repo layout

```
capture/              cargo crate -> clipper-capture.exe
capture-bin/          the built exe, shipped via electron-builder extraResources
```

`capture-bin/` is tracked in Git like `ffmpeg-bin/` but does **not** need LFS — the exe is about a
megabyte, not 130. `capture/target/` is gitignored. `main.js` has a `getCapturePath()` mirroring
`getFfmpegPath()`, which prefers `capture/target/release` when it exists so a development build is
picked up without copying anything. Rust does not build on `npm start`; you build the exe when you
change it.

### The modules

Roughly in the order a frame travels. Nothing here is a layer for its own sake: each file owns one
thing that can go wrong on its own, and most of them own a counter that says when it has.

**Getting in and staying alive**

| file | owns |
|---|---|
| `main.rs` | every subcommand, and the argument parsing for each. `record` runs the same `daemon::run` the daemon does, so what is tested by hand is what ships |
| `lifecycle.rs` | the single-instance mutex, the log file, and exiting when Clipper does |
| `daemon.rs` | the loop that owns everything else: the tick, the watchdog, the health poll, the control channel, `status` |
| `config.rs` | `capture.json`, the quality tiers it expands into, and which changes need a rebuild |
| `ipc.rs` | the named pipe, framing, and the command vocabulary |
| `clock.rs` | QPC, the fixed-rate ticker, and the 100 ns unit everything else is stamped in |
| `hotkey.rs` | `RegisterHotKey` for all four binds, and the message loop behind them |
| `gamepad.rs` | buttons and hats on game controllers, read through Raw Input for the alternative binds |

**Picture**

| file | owns |
|---|---|
| `monitors.rs` | enumerating displays and resolving the configured one |
| `display.rs` | whether a display is in HDR right now, and its SDR white level |
| `d3d.rs` | the D3D11 device everything else shares |
| `capture.rs` | the Windows.Graphics.Capture frame pool, and noticing when it closes |
| `convert.rs` + `convert.hlsl` | the only place pixels are touched: downscale, tone map and NV12 pack, in one pass |
| `encoder.rs` | H.264 through a hardware Media Foundation transform, and the rate control that decides what a clip costs |

**Sound**

| file | owns |
|---|---|
| `audio.rs` | WASAPI endpoints, the keep-alive stream, rate matching a microphone, the two-source mixer and its limiter |
| `denoise.rs` | noise suppression on the microphone: RNNoise, the strength blend, and the frame of latency taken back out |
| `aac.rs` | AAC encode, and the resampler for endpoints Media Foundation will not take directly |

**Disk**

| file | owns |
|---|---|
| `ts.rs` | the MPEG-TS muxer, written here because segments have to concatenate by byte |
| `ring.rs` | the segment ring, eviction by duration, and turning a span of segments into an MP4 |
| `record.rs` | recording on demand: a copy of the ring's packets into one growing file, carried across rebuilds, and finished — or recovered — as an MP4 |

**What is in front**

| file | owns |
|---|---|
| `foreground.rs` | is this a game, and should the recorder be running |
| `gamename.rs` | what that game is called, which is very often not what its executable is called |
| `gpuload.rs` | the per-process GPU counter, summed over a process tree |

`foreground.rs` and `gamename.rs` are split because the questions are: the first is a live
classification that changes every half second, the second is a one-shot lookup whose answer cannot
change while the same process holds the foreground.

Two environment variables exist for testing and change nothing when unset.
`CLIPPER_CAPTURE_INSTANCE` suffixes the single-instance mutex and the pipe name (Electron's
`CAPTURE_PIPE` reads it too), so a harness can run its own daemon beside a real Clipper's.
`CLIPPER_KEEP_TS` keeps a recording's `.ts` after it is published, so a timing question about the MP4
can be told apart from one about the muxer. `--stall-after` wedges only the pipeline running when the
stall begins; the one the watchdog builds to replace it runs normally, as it would after a real wedge.

`dump.rs` is behind the `dump` feature and has no part in recording — it writes PNGs so the capture
path can be checked against real pixels.

### Dependencies

`windows` for everything (Direct3D11, DXGI, Media Foundation, WASAPI, Graphics.Capture, PDH,
registry, tool-help, property store), plus `serde` and `serde_json` for the config and the control
channel. `nnnoiseless` is RNNoise for the microphone, pure Rust with its weights compiled in; its
default features are command-line tools and are off. `png` is optional and only for the `dump`
feature. There is no error crate: fallible
paths return `windows::core::Result` or a boxed error, because almost every failure here originates
in a COM call and wrapping it would only move the message.

Built with the MSVC toolchain — `x86_64-pc-windows-msvc`, Rust 1.98 — which is the right target for
COM and WinRT work.

## Milestones

Each ends in something verifiable against real media, since there is no test suite.

1. ~~**Skeleton + capture.**~~ **Done.** `clipper-capture monitors` lists displays as JSON;
   `clipper-capture dump` runs the fixed-rate pull and reports what happened.
   *Verified on a 2560x1440 primary, a 1080x1920 portrait secondary, and both pixel formats:*
   60.00 fps effective over 300 ticks, 0 missed, tick jitter mean 0.27 ms / p99 0.52 ms against a
   16.67 ms budget, `pump()` mean 0.03 ms / max 0.79 ms. PNGs match the real desktop in colour and
   orientation. FP16 output was identical to BGRA, which is the expected result with Windows HDR
   *off* — scRGB keeps SDR content inside [0,1], so the placeholder clamp is a no-op. Milestone 3
   needs HDR switched on to mean anything.
2. ~~**Encode.**~~ **Done.** `clipper-capture encode` runs capture → convert → NVENC → raw `.h264`.
   *Verified:* ffmpeg reports `h264 (High), yuv420p(progressive), 1920x1080 [SAR 1:1 DAR 16:9]`
   and decodes all 600 frames clean. 10 I-frames at exactly n = 0, 60, 120 … 540, 590 P-frames and
   **zero B-frames**, so DTS equals PTS as intended. Steady-state submit cost 0.16 ms mean /
   0.92 ms max per frame, 0 dropped ticks at 60 fps. The first submit of a fresh process costs
   ~27 ms (NVENC session warm-up); the ticker absorbs it without dropping a frame, but the daemon
   should warm the encoder before it claims to be armed.
3. ~~**Tone map.**~~ **Done, merged into 2** — HDR was already on, so building the SDR-only path
   first would have been throwaway work. *Verified* by eye at three stages: the NV12 output read
   back, and a frame decoded out of the finished `.h264`. Correct exposure (the 2.5x white divide
   working), no colour cast through the YUV round trip, text legible after 1440p→1080p.
4. ~~**Audio.**~~ **Done.** `clipper-capture audio` captures the default render endpoint through
   loopback, folds to stereo, and encodes AAC-LC.
   *Verified:* endpoint is 48 kHz stereo float, so no resampling. ffmpeg reports
   `aac (LC), 48000 Hz, stereo, 192 kb/s` and decodes clean. A 1 kHz tone pushed through the
   keep-alive stream came back out of the decoded AAC at **1000.00 Hz, RMS 0.1414** against the
   0.1414 a 0.2-amplitude sine should give — so the float32→int16 conversion is right in scale,
   not merely present. **Drift over 60 s: 0.0 ms**, sample count against the device's own packet
   timestamps, with 0 filled frames.
5. ~~**TS muxer + ring + hotkey.**~~ **Done.** `clipper-capture record` runs the whole buffer;
   `--save-after` fires a save without a keyboard so the path can be tested headlessly.
   *Verified* on a 95 s run with a 60 s buffer: 5700 video packets for 5700 ticks, **0 missed
   ticks**, 48 segments written / 15 evicted / 33 retained, 56 MB on disk. The muxer's own output
   read back by a parser that does not use ffmpeg: **0 continuity errors, 0 bad sync bytes**, every
   segment opening on a keyframe, and segment-to-segment gaps of exactly +0.0167 s — one frame at
   60 fps. The saved MP4 is `h264 (High) 1920x1080 60 fps + aac (LC) 48000 Hz stereo`, decodes
   clean, and the copy took 147 ms on a background thread without costing a tick.
6. ~~**Electron integration.**~~ **Done.** `capture.json`, the control pipe, tray with
   close-to-tray, spawn/lifecycle with `--parent-pid`, and an Instant replay panel in Settings.
   *Verified* by driving the real app: monitors listed with their live HDR state, toggling the
   switch starts the daemon and the status line goes live, `save` from the UI produces a playable
   clip in the per-game subfolder, and the daemon runs at **59.9 fps with 0 dropped and 0 missed
   ticks** while the settings panel is open and polling.
7. ~~**Living with it.**~~ **Done.** Everything that only shows up once the recorder is running for
   hours on a real machine: picture quality against what NVIDIA produces, settings that apply when
   you change them, a screen that gets switched off, HDR toggled mid-session, and a folder-per-game
   that means games.
   *Verified* by driving the real app and by measurement against real media —
   **12.1 Mbps achieved against the 720p tier's then-new 12 Mbps target** on hard content, against
   8.0 at the tier's old 8 Mbps, measured on the same 60 fps source (milestone 8 raised it again,
   to 16);
   keyframes at exactly 2.000 s in a saved clip; **+2.9 dB** on a zone-plate downscale for 0.6
   points of the 3D engine; quality changed live with **no daemon restart** and the pipeline rebuilt
   in place; HDR toggled off and on with a rebuild each way in about a second, 59.9 fps either side;
   the configured monitor removed and restored with the daemon alive and answering throughout; a
   clip taken with Electron in front filed under `Desktop` rather than `electron`.
8. ~~**Cost measurement, and names.**~~ **Done.** What milestone 7 spent on quality, measured,
   priced, and mostly given back; and the folder a clip lands in named after the game rather than
   after its executable.
   *Verified* by sweeping the preset against the video-encode engine on a hard 60 fps source, by
   encoding real gameplay through the same silicon at every NVENC preset, and against every game
   Windows has on file on this machine — **encode cost roughly halved at every tier** (720p
   10.1% -> 5.3%, 1080p 22.5% -> 12.4%, 1440p 46.5% -> 21.0%) while the 720p tier gained 4 Mbps,
   which is worth about five times what the preset drop cost; and **22 of 22 registered games
   resolved to a name a person would recognise**, including `RuntimeClient-Win64-Shipping` ->
   `REMATCH` and `PenguinHotel-Win64-Shipping` -> `MECCHA CHAMELEON`. A clip saved from a fixture
   installed as a Steam game landed in `Zonewalker - Director's Cut`.
9. ~~**The microphone.**~~ **Done.** A capture endpoint mixed into the same stereo track as the
   desktop, on by default, with a device picker that can also just follow the system default.
   *Verified* against real endpoints — **0.0 ms drift over 10 s** between the two device clocks and
   0 dropouts; the desktop leg unchanged through the mixer (a 440 Hz tone reads 440.01 Hz without
   the mic and 439.99 Hz with it, at identical RMS); the mic alone carrying signal through the sum
   (non-zero RMS from a silent desktop); a microphone that cannot be opened logged once, reported
   in `status`, retried, and costing the recording nothing — 386400 desktop frames at 0.0 ms drift
   while it was missing; and a saved clip whose audio track runs its full length
   (1210368 frames against 25.23 s of video).
10. **In-game frame time.** The GPU-engine figures above are not the requirement; frame time in a
   real game, with and without the recorder, is. Still unmeasured, and the one number nobody
   should take on trust.

## What the implementation corrected

**The keep-alive stream is load-bearing, and there is no fallback.** Measured on a quiet desktop:
without it, 15 seconds of capture yields **zero samples** — not thin audio, none at all. With it,
15.04 seconds. The gap-fill described above cannot cover this case either, because gap detection
needs a packet to anchor from and there are no packets. So the render stream is the primary
mechanism and gap-fill only patches holes *between* packets. `--no-keepalive` exists to demonstrate
this rather than leave it as a claim.

**AAC has encoder delay.** The decoder returned 640 samples (13.3 ms) more than went in, which is
ordinary AAC priming. It is under a frame at 60 fps but not nothing, and the muxer has to either
trim the priming samples or honour the per-sample PTS the MFT already supplies — otherwise every
clip carries a fixed ~13 ms audio lead.

**The device sample clock and QPC agree.** This was the failure worth fearing — a slow divergence
that only shows up as lip-sync sliding near the end of a long clip. Measured at 0.0 ms over a
minute, so no rate correction is needed. Worth re-checking on other hardware.

**`CODECAPI_AVEncCommonRateControlMode = CBR` is a ceiling, not a floor.** Measured on the NVIDIA
MFT: a 2 Mbps target produced 1.70 Mbps, and a 50 Mbps target produced 7.03 Mbps on the same
content. The knob clamps but does not pad. This is harmless — eviction is by duration, not bytes,
so the ring still holds exactly the configured window — but the disk figures quoted above are an
upper bound, not an estimate.

**NVENC already repeats SPS/PPS at every IDR**, and emits an access unit delimiter before every
frame. Both are exactly what the TS muxer wants, and the AUDs make access-unit boundaries trivial
to find. The muxer must still be prepared to insert the sequence header itself, because this is a
per-vendor behaviour and not a guarantee — but it should dedupe rather than blindly prepend.

**The converter needs a pool of NV12 surfaces, not one.** An async MFT reads the texture it was
handed on its own schedule and gives no signal when it is finished, so writing the next frame into
the same surface can corrupt the frame being encoded. Four surfaces costs a few megabytes.

**NV12 plane render targets work** on this hardware (`CheckFormatSupport` confirms, and the output
is correct), which is what allows the shader to write the encoder's own texture in place instead of
going through a scratch buffer and a copy.

## What milestone 5 corrected

**Segments were three seconds, not two, and nothing said so.** Rotation compares elapsed ticks
against a target, and after two integer divisions a keyframe meant to land exactly on the two
second mark computes as 179_999 against a threshold of 180_000. The test failed, rotation waited a
further keyframe, and every segment came out 50% longer than configured — which in turn inflated
both disk retention and the head-rounding on every saved clip. Fixed with a 20 ms tolerance, tiny
against the one-second keyframe interval so it can never rotate early. Worth remembering as a
shape: **a threshold compared against a derived integer timestamp needs slop**, or it fails half
the time and silently.

**Eviction measured from the wrong end.** It compared against the newest *closed* segment rather
than the one being written, so the ring held one extra segment beyond the window. Retention on
disk is now `window + 2 segments of slack + the segment in flight` — about 66 s for a 60 s buffer.

**The A/V offset does not accumulate.** Audio runs ~13-32 ms shorter than video in a finished clip:
AAC priming at the head, a partial frame at the tail. Measured -32.0 ms on a 12 s clip and
**-12.7 ms on a 61 s clip** — it shrinks rather than growing, which is what proves it is a boundary
effect and not clock divergence. Audio starts 14-18 ms after video, comfortably inside the range
where a lag is imperceptible, and can be removed later by shifting audio PTS back by the encoder
delay if it ever matters.

## The bug that milestone 6 existed to find

**A named pipe without `FILE_FLAG_OVERLAPPED` throttled the recorder to the rate the UI polled.**

Windows serialises I/O on a synchronous handle. The pipe thread sits in a blocking `ReadFile`
waiting for the next command, and that pending read blocks any `WriteFile` on the same handle —
so emitting an event from the recording loop could not complete until the UI happened to send
another command. The recorder therefore advanced roughly one frame per command received.

What makes it worth writing down is how well it hid. Every part looked healthy: `missedTicks` was
0, the encoder reported no dropped frames, the pool never rebuilt, capture sizes matched, status
replies arrived, and clips were produced. Standalone runs were perfect, and so were runs spawned
with the exact same flags from plain Node — it only appeared when something was on the other end of
the pipe reading and writing. The first diagnosis attempt was also wrong twice over: `main.js` was
returning a *cached* status, so a wedged daemon looked alive with plausible numbers, and an
instrumentation field was seeded with the value it was meant to test.

What actually found it was logging a timestamp at the top of each loop iteration and around the
tick, which showed the tick completing in microseconds and eleven seconds vanishing *between*
iterations — pointing at the code after the tick rather than anything in the pipeline.

Three things came out of it and are kept:

- **The status reply is real, not cached.** A daemon that has stopped answering is precisely the
  failure worth seeing; a cache hides it behind numbers that look fine.
- **`submit` has a deadline.** It used to block forever waiting to be asked for input. Now it
  waits a few frame intervals, drops the frame, and counts it. A recorder that wedges while still
  reporting itself as recording is far worse than one that loses a frame.
- **A heartbeat in the log.** A background process with no window has to leave evidence of its own
  health, or "it recorded almost nothing" is unfalsifiable afterwards.

Also fixed along the way: an encoder candidate that fails to configure is now shut down properly
rather than left holding a vendor encode session, and an empty `outputPath` resolves to the user's
Videos folder instead of the daemon's inherited working directory.

## What milestone 7 corrected

**The bitrate was never the ceiling anyone thought it was.** Milestone 5 recorded that CBR "clamps
but does not pad" — a 50 Mbps target producing 7 Mbps — and filed it as harmless. It was, but only
because the content was a still desktop. On genuinely hard content the encoder hits its target
exactly: 12.1 Mbps against a 12 Mbps ask, 8.0 against 8. So the tier number *is* the quality knob on
the footage anyone cares about, and 720p was set at 8 Mbps. It is now 12 mean / 20 peak, and the
measurement to take when this looks wrong again is on moving content, not on a desktop.

**`ICodecAPI` settings applied after `SetOutputType` are accepted and ignored.** `SetValue` returns
success for `CODECAPI_AVEncMPVGOPSize` on the NVIDIA MFT and the encoder then pins IDRs at one a
second regardless: 20 keyframes in 20 seconds with the property set to 120 frames. Moving the same
calls before `SetOutputType` produces exactly the 120-frame spacing asked for. They are now applied
twice, before and after, because other vendors' transforms reset codec state when the type changes.

The general shape is worth keeping: **a setter that returns success has not necessarily set
anything.** Which is why `Encoder::applied` exists and why the log names every knob that took —
a driver quietly ignoring one of these is invisible otherwise, and cost an evening here.

**A four-tap "Catmull-Rom" is not a Catmull-Rom.** The first attempt at the downscale filter took
four bilinear taps a destination pixel apart, which is the shape the standard optimisation has and
none of its substance: at 2x each inner tap straddles a texel boundary, so the weights land flat
across four source texels instead of peaking over two. It measured *blurrier* than the box filter it
replaced. Sampling the kernel at the source grid is the only version that is the filter it claims to
be — and the way to tell is a zone plate, because on ordinary test images with little energy near
Nyquist all three versions measure within 0.05 dB of each other and look identical.

**Restarting the daemon to apply a setting could not work, and looked like it did.**
`stopCaptureDaemon()` returned as soon as it had asked the old daemon to quit, but the old daemon
holds the single-instance mutex and the named pipe until it has finished flushing its ring — so the
replacement spawned 300 ms later found the mutex taken and exited on the spot. Nothing was
recording, and the settings panel went on quoting a process that no longer existed.

The fix is not better timing. The daemon can already rebuild its own pipeline, so the process is now
started and stopped for exactly one reason — the feature being switched on or off — and every other
change goes down the pipe as `reload`. A `reload` is answered with a fresh `status`, so the panel
redraws immediately instead of waiting for its next poll, which is the other half of what
"the settings are not applying" felt like.

**Covering the screen is not a game.** The original heuristic gave a folder in the clips library to
Telegram, Windows Terminal and a full-screen browser, which is not what "a folder per game" means to
anyone. Two signals fixed it, both of which Windows already maintains and neither of which we had
thought to ask: `HKCU\System\GameConfigStore\Children`, where Game Bar records the executables it
has identified as games, and the per-process 3D-engine counter behind Task Manager's GPU column.
Non-games now share one `Desktop` folder instead of each getting their own.

Two things only showed up once it was driven against a real full-screen process that nobody had
listed. **The GPU counter attributes to the process that does the drawing, not the one that owns the
window** — a Chromium-based full-screen window measured 0% until the load was summed over its
process tree. And **one flag cannot answer both questions**: while `is_game` also gated recording,
every choice of threshold traded lost footage against unwanted folders. Splitting it into a strict
`is_game` for the folder and a generous `worth_recording` for the recorder removes the trade
entirely, and is what let the threshold come down to ten percent without anything being at stake.

**The stream was never actually tagged bt709.** The tone map has always re-tagged its output in
*intent* — the design says so twice — but nothing set `MF_MT_VIDEO_PRIMARIES` and friends, so the
encoder emitted no VUI colour description at all and ffmpeg reported a bare `yuv420p`. Harmless in
practice, because every player assumes bt709 for HD, and precisely wrong in the one case this
pipeline exists for. It now reports `yuv420p(tv, bt709, progressive)`.

**A wrong audio timestamp from the driver is indistinguishable from a bug in our clock.** A/V sync
rests on `GetBuffer` handing back the QPC value the first sample of a packet was captured at, and on
a report of a second of audio delay that a reboot cured there was no way to tell whether the device
had lied or we had. `Loopback::anchor` now checks the first packet's stamp against the capture clock
— a packet arriving now was captured at most one endpoint buffer ago and certainly not in the future
— and removes a constant offset when it is somewhere it cannot honestly be, logging that it did. The
rate was measured as correct to 0.0 ms over a minute, so only the offset is ever touched. The
running offset and the audio track's start are both in `status` and in the heartbeat, measured at
+2.9 ms to +18.5 ms across these runs, so next time the question is answerable rather than arguable.

## What milestone 8 corrected

**"The encode chip is idle, so the preset is free" was a guess, and it was wrong.** Milestone 7 set
`AVEncCommonQualityVsSpeed` to 80 on that reasoning and never measured it. On the NVIDIA MFT this
maps onto three preset bands whose cost roughly doubles at each step, and 80 is the top one:

| quality-vs-speed | 720p | 1080p | 1440p |
|------------------|------|-------|-------|
| 0-16             | 3.2% | 6.6%  | -     |
| 33-50            | 5.0% | 11.1% | 19.6% |
| 66-80            | 10.1%| 22.5% | 46.5% |

(Video-encode engine, 16 s of a hard synthetic 60 fps source.) That is what turned 1080p from about
10% of the GPU into about 30% of it, which is not a price anyone agreed to pay for a recorder that
is supposed to be invisible while you play.

**What the top band buys is a quarter of a dB.** Encoding twenty seconds of real gameplay at
12 Mbps through the same silicon, NVENC's own presets span **0.71 dB PSNR from p1 to p7** — and p6
to p7 is nothing at all. Meanwhile **12 to 16 Mbps at p4 is worth 1.25 dB**, five times what the
whole preset ladder gives, and costs disk instead of the GPU the game is using. So the middle band,
and four more megabits at 720p: better picture and half the encode cost, in the same change.

The general shape is the one milestone 7 kept running into from the other side. *A knob that is
cheap in principle is not cheap until it is measured*, and the way to find out is to sweep it
against a counter rather than to reason about how busy a chip ought to be.

**The Catmull-Rom downscale is not where the cost is.** Worth recording because it was the obvious
suspect: it costs **half a percentage point of the 3D engine** — 1.13% against 0.63% at 720p from
1440p — which is the engine the game actually wants, but is nowhere near large enough to matter.
It stays on.

**An executable's name is not the game's name, and Unreal makes that the normal case.** REMATCH
files clips as `RuntimeClient-Win64-Shipping`, which is what the user saw and reported. The fix is
not cleverness about strings: Steam already stores the product name beside the install directory in
`appmanifest_*.acf`, launchers already name their install folders after the game, and Unreal's
layout already puts the install root two directories above `Binaries`. Reading what is already
written down gets all 22 games registered on this machine right, including three — `hlvr`, `acs`,
`sekiro` — where nothing about the file name resembles the product.

Two small wins came free with it. `AssettoCorsa.exe` and `acs.exe` are the same game, and now share
one folder instead of two. And the name survives a rename of the executable, which the file-name
answer did not.

## What the microphone corrected

Reported as "it records, but when I speak it sounds like there is interference". Not reproducible
here — a quiet room gives the mixer nothing to get wrong — so it took a clip from the person who had
it, and the clip said it plainly. Rendered as a spectrogram it is covered in **vertical full-band
stripes**: sample-level discontinuities, about eight a second, many of them spaced almost exactly
120 ms apart. Peak level was -23 dBFS, so nothing was ever close to clipping.

**The cause was the gap filler, doing exactly what it was written to do.** Loopback delivers nothing
while the audio engine is stopped, so `Endpoint::poll` compares each packet's timestamp against
where the last one ended and inserts silence to cover anything over half a millisecond. That is
right for the render endpoint and wrong for a microphone. A USB microphone's packet timestamps
*jitter*: a packet routinely lands a fraction of a millisecond later than the previous one ended,
and the next one comes back. Filling only ever adds, never removes, so every excursion past the
threshold punched a **24-sample hole of digital silence into the middle of the voice**. Measured on
this machine: **5264 frames of invented silence in twenty seconds — one hole every 92 ms**, against
the 120 ms spacing measured in the clip.

The fix is to stop pretending the two clocks are the same one. A microphone is now **rate matched**:
the difference between where the device says a packet belongs and where our output has reached
drives a first-order loop that resamples by a fraction of a percent, instead of a threshold that
punches holes. One second of time constant, one percent of authority, linear interpolation — at a
ratio of 1.0005 the interpolation is between samples 20 µs apart, where speech is very nearly a
straight line, and a windowed-sinc kernel would buy nothing audible for a great deal more
arithmetic. Silence is still the answer past 50 ms, because past 50 ms it is a real hole.

Measured after: **filled frames 0**, drift 0.01 ms over twenty seconds, and a fresh recording with
**0 discontinuity events** against 163 in the clip that was sent. The loopback leg kept the old
path, on the grounds that it did not have the problem — its clock matches the capture clock to
within 7 parts per million, where the microphone's is a few hundred. That was true of this machine.
It turned out not to be true of every machine: see the next section.

Two more faults came out of going looking, neither of them the cause, both worth fixing:

**Unity summing with a clamp is not a policy, it is the absence of one.** Two sources summed into a
fixed-point track reach full scale as soon as both are loud, and `clamp` there flattens the peaks of
whichever signal anybody cares about. The sum now goes through a lookahead limiter: nothing at all
while there is headroom — `cargo test` holds it to bit-identical output — and a fraction of a
decibel of gain movement when there is not. It was *not* what this bug was, and the clip proves it:
nothing in it came within 23 dB of the ceiling.

**A source that joins late was being shifted rather than waited for.** The microphone opens after the
loopback and its first packet lands about 80 ms later. `Track::take` read from the front of whatever
buffer it had and placed those samples at the current point in the timeline, putting every one of
them early by that 80 ms — then spent the next minute dropping samples a few at a time to work the
error off. Measured before: 3840 frames padded and 2548 dropped in fifteen seconds, both still
climbing. Now a source is silent until the moment it actually began: padding 0, and the drop count
flat between a fifteen-second run and a thirty-second one.

**A mono microphone went entirely to the left ear.** `downmix_weights` fell through to the stereo
weights for a one-channel endpoint, so the single weight went to left and nothing filled right.

Three general shapes worth keeping. **A fault in a sum cannot be attributed by looking at the sum** —
every counter this section leans on had to be added before any of it could be seen, and the per-leg
`--wav` dump exists for the same reason. **A defence written for one source is not automatically
right for another**: the gap filler was correct code applied to a device it was never designed for.
And **a hypothesis that survives only because it has not been measured is not evidence** — the
limiter was built on a clipping theory that a single look at the clip's peak level disproved.

## What the desktop audio corrected

Reported, after the microphone fix, as the same thing in the other leg: "the desktop sound on the
clip is crackling sometimes", on a different machine from the one that had the mic problem.

**The loopback leg still had the rule the microphone had lost.** Any packet stamped more than half a
millisecond later than the previous one ended got silence in front of it. On this machine that rule
never fires: loopback packet timestamps scatter by **±0.02 ms** and filled frames read 0. It is
only safe on an endpoint that keeps time that precisely, and nothing promises that every render
endpoint does — a USB or wireless headset has a crystal of its own, and a virtual mixer between the
game and the speakers has a scheduler of its own. Which is also why it could not be heard here. A render endpoint that jitters by a few milliseconds gets the
microphone's crackle in the game audio: in the unit test fixture, ±3 ms of jitter trips the old rule
on well over five hundred packets in twenty seconds. The old rule also looked only from one packet
to the next, so a render endpoint on a crystal of its own never tripped it at all and simply let
the game audio slide against the video.

Both legs now go through one `Pacer`, which tells jitter, drift and holes apart and answers each
differently. The desktop leg's 10 ms deadband is what keeps this a change for the machines that had
the problem and nobody else: inside it the ratio is exactly one and the output is bit-identical to
what the old path produced, which `cargo test` holds it to. Past it, the resampler is **cubic**
rather than the microphone's old linear one. Game audio is not speech: linear interpolation halfway
between two samples is 3 dB down at 12 kHz, and as the read position slides through each sample
that dulling comes and goes several times a second. Cubic holds it to 1 dB.

`status` gains `deskFilled`, `deskClockPpm` and `deskRatePpm`, and the heartbeat line in
`capture.log` carries the desktop clock whenever it is more than 50 ppm out or anything was filled.
Before this, a desktop leg in trouble left no trace anywhere a user could send. It is also the
evidence this section is still missing: the fix follows from the mechanism and the fixture, not yet
from a clip or a log off the machine that had it.

## What the bitrates corrected

Reported as "a minute of REMATCH at 720p is 200 MB, and a hardware recorder does the same game in
about 50". Both numbers were right, and the gap was not subtle.

**The peak allowance had quietly become the bitrate.** The 720p tier asked for 16 Mbps mean and
allowed 26 on a hard scene, on the reasoning that a spike is brief and the average is what you pay.
That reasoning holds for content which is *sometimes* hard. A football match is hard from the first
frame to the last, so the encoder simply sat at the ceiling: the clip measured **25.9 Mbps**, 60%
above the number anybody had chosen. Their earlier clip at the 12/20 tier measured 12.7 Mbps, right
at its mean, which is why this went unnoticed until the content got harder. The ratio is now about
1.35, so the worst case is close enough to the promise to be one.

**And the means were set before the encoder was fixed.** 720p was raised from 8 to 12 to 16 Mbps
across milestones 7 and 8, chasing a picture problem whose real causes turned out to be the
low-latency preset, a one-second GOP and a box-filter downscale. With those fixed, the same bitrate
buys **+1.2 to +1.6 dB** — measured on twenty seconds of the reporter's own gameplay, old settings
against new at matched bitrates:

| asked | before | now |
|-------|--------|-----|
|  6 Mbps | 45.21 dB | 46.39 dB at 5.3 Mbps actual |
|  8 Mbps | 46.63 dB | 48.13 dB at 7.2 Mbps actual |
| 10 Mbps | 47.71 dB | 49.30 dB at 8.9 Mbps actual |
| 12 Mbps | 48.63 dB | 49.97 dB at 10.1 Mbps actual |

So 8 Mbps now looks like 11 did, and the tier that was raised to fix the picture can come back down
without giving the fix back. Measured after, on the hardest source available: 720p **9.2 Mbps, 69 MB
a minute**, against 16.4 Mbps and 123 before.

**The codec was checked and is not the answer.** HEVC and AV1 were measured on the same content at
the same bitrate and came out 0.2 to 0.9 dB ahead of H.264 — real, but nowhere near the four-fold
difference being explained, and not worth what they cost in places that will not play them.

The shape worth keeping: **a number chosen to fix one problem stays after the problem is fixed
elsewhere.** Every bitrate rise here was a reasonable response to a real complaint, and all of them
were still in place after the actual causes had been found and corrected.

## What switching outputs corrected

Reported as "when a default output device changes, the capturing process stops capturing audio from
desktop". The design above said an `IMMNotificationClient` would restart the audio leg when the
default moved. It was never written: the loopback opened whatever was the default when the
pipeline was built and held it until something else caused a rebuild.

**Holding the old device fails silently.** Switching from speakers to a headset leaves the speakers
present and working, so the loopback on them never errors. It keeps delivering packets of nothing,
and the keep-alive stream keeps the old device's engine running, so none of the gap detection has
anything to see either. Reproduced by moving the default between two virtual outputs while
`clipper-capture audio --wav` ran: the shipped build recorded **5.0 s of digital silence** for the
five seconds the default was elsewhere, with whatever was playing still audible on the new device.

The desktop leg now does what the microphone left on "default" already did. Every 250 ms the mixer
asks for the default render endpoint's id. When the id moves, the old stream and its keep-alive are
dropped and the new default is opened on the same poll. An error from the device, whether in
`poll` or in the keep-alive's `pump`, is handled the same way: the leg is dropped and the default is
retried every second. Before this, that error propagated and cost a pipeline rebuild after thirty
ticks. The ring, the video and the microphone are not touched either way.

**The new device's samples have to land at their own time.** On the pass-through path, with no
microphone, a fresh `Pacer` would start its timeline at its first packet. The new device's audio
would then follow straight on from the old one's, the gap would vanish from the track, and every clip
after a switch would have its audio early by however long the switch took. So the new pacer is
*resumed* at the old one's position (`Pacer::resume`). The gap becomes a hole like any other and is
filled, and a first packet stamped before the handover is trimmed rather than stretched back over
seconds at one percent. While there is no device at all, the pass-through path emits silence on the
clock, 200 ms behind the present, so the track keeps pace with the video instead of stalling and
arriving all at once. The mixed path needed less: it already lines each track up by timestamp, so the
desktop track starts over, and the mix falls back to the clock only when no source is delivering.

**Rate.** A device at a different rate from the mix is opened through the audio engine's converter
(`AUTOCONVERTPCM`, as the microphone does), which works in loopback as well: a 48 kHz endpoint read
at 44.1 kHz delivered 88,608 frames in 2 s. If a driver refuses the conversion, `desk_rate_changed`
makes the pipeline stale, and it is rebuilt at the new rate. That is the one case that costs the
buffer.

Measured through the whole app, with the default moved Realtek → Pimax → Realtek inside one buffer:
the saved clip has no silent 250 ms window at all, and audio ends 8 ms from the video. The virtual
Oculus device was slower, with 0.5–2 s of silence at a switch. That device takes about a second to
start again once released (the old build never released it), and its first packet comes back stamped
a second stale, which `anchor` corrects. `status` gains `deskDevice` and `deskError`, and the panel's
desktop-audio tip names the device being recorded, because that can now change under it.

## Known limits

- **Only the default output is recorded.** Audio an application sends to a device of its own
  choosing — voice chat pinned to a headset while the game plays on speakers — is not in the clip.
  Every output at once would need either a loopback per device, each on its own clock and mixed, or
  process loopback (`ActivateAudioInterfaceAsync` with `PROCESS_LOOPBACK` excluding Clipper's own
  tree, which would also keep the replay-saved sound out of the next replay). Neither is built.

- **Elevated games swallow the hotkey.** `RegisterHotKey` from a normal-integrity process never
  sees keys while an admin-elevated game has focus. A low-level keyboard hook has the same
  limitation. The only real fix is running the daemon elevated, e.g. via a scheduled task at logon.
- **Protected content captures black** (Netflix, some DRM overlays). By design, unavoidable.
- **Anti-cheat.** WGC is a documented Microsoft API used by OBS and Xbox Game Bar and we inject
  nothing, so this should be fine — flagged because "should be fine" is not "verified".
- **33-bit PTS wraps** after about 26.5 hours. MPEG-TS and ffmpeg's demuxer handle wraparound, but
  a daemon left running for days crosses it, so it is worth testing by starting the clock near the
  wrap point rather than waiting a day to find out.
- **One microphone, and its boost is a plain gain.** `micGainDb` multiplies, so without
  `noiseSuppression` it lifts the room noise with the voice; there is no gate. The limiter stops a
  boosted mic from clipping the sum, and `micPeakDb` in `status` is what the panel shows so the level can be
  set by looking rather than guessing. Two microphones, or a mic on its own track, are not offered.
- **The first 60 ms of microphone audio is discarded.** The loopback opens first and the mic's first
  packet is timestamped slightly before the point the mix had already reached, so that overlap goes.
  It is one discard at the start of a session and nothing after it.
- **Suppression folds the mic to mono.** The network runs once, on the average of the two
  channels, and writes the result to both. A microphone is mono in all but name — the endpoint
  duplicates it — but a genuinely stereo one loses its image with suppression on.
- **Exotic audio endpoints.** 5.1 and 7.1 fold down through the endpoint's channel mask, weighting
  centre and surrounds at -3 dB rather than the easy implementation's "keep front L/R", which would
  silently drop dialogue. 88.2 and 96 kHz endpoints route through Media Foundation's resampler.
  Neither path has been exercised on real hardware — this machine is 48 kHz stereo.
- **The downscale filter is narrowed past 2x.** Its support is capped at 8 taps per axis, which is
  exact at 2x — a 1440p panel at 720p — and short of the full Catmull-Rom footprint at, say, 4K to
  720p. A narrow kernel is the right failure: sampling a wide kernel sparsely aliases, which is what
  the filter is there to stop.
- **A rebuild costs the buffer.** Switching HDR, changing quality or losing the display for a
  moment all start the ring over. That is the intent for HDR — you want the clip that follows the
  switch, not one spliced across it — and unavoidable for the others, but it does mean a monitor
  that drops out for a second costs you the minute behind it.
- **The first second after the recorder starts has no audio.** WASAPI takes a moment to deliver
  its first packet, so a clip saved within the first few seconds of the buffer beginning starts
  with silent video. A buffer that has been running for a minute never includes that region.
- **A game that barely touches the GPU gets recorded but not filed.** The classifier wants ten
  percent of the 3D engine sustained for a second and a half before it will name a folder after a
  full-screen window. Something very light — an old 2D title, a game capped at 30 fps on a fast card
  — can sit under that until Windows has it on file in `GameConfigStore`, and its clips land in
  `Desktop` in the meantime. It is still recorded, because `worth_recording` is the generous half of
  the pair. The settings panel says what it decided and why, and "treat as a game" fixes it in one
  click; there is no threshold that is right for every machine.
- **The GPU counter is a diagnostic API and may be missing.** If PDH will not open the counter set,
  the classifier falls back to the old full-screen rule and says so in `status`.
- **The hotkey works** (confirmed by hand), but not yet against an elevated game, which is the
  case Windows will not deliver.
- **Quitting Clipper mid-recording can leave the MP4 for the next start.** The daemon finishes the
  recording on `quit`, but Electron kills it two seconds later and the parent watcher ends it the
  moment Clipper exits; a long recording's remux takes longer than that. The `.ts` is complete, and
  `record::recover` publishes it the next time the daemon starts.
- **A recording started with replay off loses its first ~0.8 s** to building the pipeline. With
  replay on it starts on the keypress.
- **Raw `.h264` carries no frame rate**, so ffmpeg guesses 25 fps when probing the milestone-2
  output. Nothing is wrong with the stream; timing arrives with the muxer.
