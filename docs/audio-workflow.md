# Microphone mixing and synchronization

FilmCraft keeps recordings intact and puts microphone work through inspection, a proposal,
short before/after auditions, owner approval and verification. Open **Essential Sound → Channels
& Tools** for native controls. Agents use the typed audio MCP tools or the commands below.
All clip identities are timeline audio clips; a source item and a timeline clip are different.

## Choose the recording first

A camera container can carry several native audio streams, and a companion WAV can carry several
channels within one stream. `audio.channels.get` reports both. `audio.source.set` selects a clip's
native stream and source-channel map without changing the recording. For a four-channel WAV,
`channels:[2,3]` explicitly chooses its second pair; indices are zero based. Do not discard the
other pair or infer a physical microphone from stream order, filename, or signal level.

`audio.streams.configure {clips:[mainClip],mainStream:1,muteSecondary:true}` enables that stream
in the selected linked occurrence and retains its other container streams muted. A single audio
clip selection does not silently repair its linked camera backup. An explicit video selection
can reach its linked audio. Ambiguous unlinked copies are not a shared occurrence.

The automatic footage proposal accepts `mainAudioStream` (default0) and `secondaryAudio` (`mute`
by default or `keep`). Missing requested streams explicitly fall back to0 with a warning.
Channel-map placement is accounted for when choosing the created clip identity. Music gets its
own track after the source's audio tracks. Main-stream choice is also exposed in Agent Review.

## Repair, clean up and balance

`audio.prepare` stages `routing`, optional `preset`, and optional normalization against the
current or specified `baseRevision`. `audio.channels.set` supports left-to-both, right-to-both,
mono average, swap, polarity, independent output gain and mid/side utilities. Copying a mono mic
to both speakers produces centered dual mono; it does not invent spatial stereo.

Automatic routing requires complete bounded analysis and strong evidence that one channel is
near silent while the other contains usable signal. Genuine stereo, dual mono, inversion,
silence and uncertainty are left alone. Manual routing remains available. Existing source
maps and user channel mixers are preserved by a distinct channel-router effect. A partial patch
preserves untouched parameters and animation. Replacing affected keyframes requires
`replaceAnimation:true`.

Conservative `dialogueClean`, `dialogueWarm` and `podcast` presets use native rumble/noise,
sibilance and dynamics processing. `musicGentle` preserves the sound rather than treating it as
speech. These are editable DSP settings, not learned restoration or guaranteed wind removal.

`audio.normalize` measures the complete isolated post-effects clip and adjusts its output Volume,
including every existing Volume keyframe offset. It preserves the original clip gain and
animation shape. Gain is bounded by the requested maximum, measured true-peak ceiling and
Volume range. Inactive/bypassed Volume is rejected. Silent/ungated audio is skipped. It can
report an unattainable loudness target when the peak ceiling wins. Limits are16 clips and300
seconds per clip per request. Automatic edits split bounded batches and report longer skips.
The sum of microphones, music, transitions and master effects can still exceed a per-clip
ceiling: inspect the sequence and verify the rendered file.

## Voice tone and microphone mix proposals

`audio.voice.polish` provides original, editable neutral/warm/bright recipes with an intensity
control: native parametric EQ, four-band dynamics and a final limiter. Repeated calls update
only the recipe's owned instances. User effects and unrelated animation remain in place.

`audio.mix.inspect` and `audio.mix.prepare` take explicit disjoint `main`, `backup` and `music`
clip lists. The proposal retains backup clips muted, can repair confident one-sided input,
polishes chosen voices, normalizes measurable full clips and optionally writes music ducking
from only those main microphones. Sources stay positioned. Supplying music IDs defaults
`duckMusic` to true and proposes new ducking; set `duckMusic:false` to preserve music Volume.
Replacing existing affected automation also needs `replaceAnimation:true`. Mix preparation also
defaults `repairChannels`, `normalize` and `finalLimiter` to true. Each can be disabled explicitly.
Use `audio.preview` with the returned plan and `compare:true` on the sequence to audition the
actual mixed result. Approval commits the plan as one undo step and stale plans refuse to apply.

The user-supplied reference was The Aperture Life's
[DJI Mic 3 audio walkthrough](https://www.youtube.com/watch?v=DApZ77agrsA&t=445s): EQ around7:25,
multiband processing around9:52 and limiting around13:00. Its ideas informed an original native
recipe, not a copied commercial plugin preset. The transcript's “low pass” around70Hz describes
removing low rumble, so FilmCraft uses a high-pass filter. Aggressive fixed boosts and fastest
compression are not universal defaults; brightness can raise hiss/sibilance. Limiter settings
are sample-peak controls; measured true-peak/output verification remains authoritative. No
post-limiter boost is blindly used to chase a meter toward zero. The full downloaded transcript
is private local reference material and is not shipped.

## Sync the external mic to the camera

`audio.sync.inspect` compares the camera's chosen timeline audio clip with the external microphone.
It uses full-resolution48kHz waveform evidence, including a shared clap, and reports the source
channel, offset, coverage, confidence and ambiguity. It independently checks aligned correlation
and rejects repetitive tones/loops as insufficient evidence. Confidence is heuristic, not proof
of a unique match. For a multi-stream camera, choose an explicit audio clip as the reference.
A bare video reference uses secondary stream1 when present and reports that default; this does
not identify its microphone.

`audio.sync.prepare {reference:cameraAudio,clip:externalMic}` stages the alignment. Camera position
stays fixed; target linked partners move together. Locked tracks, collisions and touching
transitions are refused. A negative target start trims the exact leading source duration and
reports the loss. Existing effects/keyframes remain intact. The current alignment supports
normal-speed, forward sources; retimed/reversed clips require an explicit separate workflow.

For ambiguous waveforms use `method:"manual"`, `referenceTime` and `clipTime`: both anchors are
exact **source** ticks (254016000000 per second), not timeline positions. `offsetSamples` is an
optional known hardware delay correction at48kHz. Check short before/after audio and frames
before approval. Single-offset alignment does not correct long-take clock drift.

## Formats, monitoring and evidence

Native WAV supports integer PCM24 and IEEE float32. Internal processing and float WAV previews
preserve finite samples above unity; integer exports require appropriate gain/limiting to avoid
clipping. Floating-point headroom does not recover analogue/ADC clipping or already clipped AAC.
DJI lists both24-bit and32-bit float internal recording modes in its
[Mic3 specifications](https://www.dji.com/mic-3/specs). A camera wireless recording is a separate
encoding path; inspect the actual file rather than assuming it retains the internal WAV format.

See [audio-analysis.md](audio-analysis.md) for measurement boundaries and async jobs. Previews are
bounded float WAVs, not source rewrites. Hardware playback can clip above unity; reduce monitoring
gain. The test harness `scripts/test-audio-workflow.py` uses external ffmpeg/ffprobe only as fixture
generators/oracles and records full original-file hashes before and after. Test media, private
transcripts and exports remain outside the repository.
