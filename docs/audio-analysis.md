# Audio diagnostics and audition

`audio.inspect` measures decoded audio without changing the project, playhead, selection, or
source files. `audio.inspectAsync` performs the same work in a cancellable background job.
`audio.preview` returns a short inline stereo float WAV for listening; it does not write a file.

```json
{"subject":{"target":"clip","clip":42,"sequence":7,"effects":"post"},"start":1270080000000,"end":1524096000000}
```

`subject.target` is `source`, `clip`, or `sequence` (also inferred from `item` or `clip`). Source
subjects require `item`; clip subjects require `clip`. A sequence defaults to the open sequence.
`stream` selects a native audio stream for a source subject only. Timeline clips retain their
stored `audio_stream`, source-channel selection, speed, reverse, and source-in mapping.

All ranges are half-open integer FilmCraft ticks (254016000000 per second), in source media time
for a source and sequence time for a timeline subject. Source subclips keep their parent media
time bounds. Inspection defaults to the subject's first 30 seconds and accepts explicit windows
up to 300 seconds. Preview defaults to the first five seconds and accepts at most ten seconds.
Each response declares its requested and actual sample-grid bounds, 48000 Hz rate, snapshot
revision, source/timeline positions, and whether the range covers the entire subject. Sample-grid
end rounding can exclude a fractional final sample. Work uses fixed 4096-frame blocks, at most
64 decoded channels and 268435456 channel-samples. Longer/more numerous channels require smaller
windows. Request shape, arithmetic, finite samples, stream availability and buffer dimensions
are validated. Missing/offline/unreadable audio is an error rather than a silence result.

The processing boundary is explicit:

* Source: selected native stream, including every decoded channel; timeline effects/gain do not apply.
* Isolated clip `post` (default): source mapping, speed/reverse, clip gain, clip effects, Volume,
  Channel Volume, and Panner. Track/master mixing and transitions are excluded.
* Isolated clip `pre`: clip effects, gain, Volume/Channel Volume/Panner are bypassed. Existing
  source-channel mapping and speed/reverse remain in force.
* Sequence `post`: the stereo monitor mix, including track, submix and master processing.
* Sequence `pre`: clip gain/effects and track/submix/master inserts are bypassed; existing faders,
  pans, transitions and channel mappings remain in force. This is not a collection of raw sources.

`sourceFormat` reports decoder-provided codec, bit depth, native sample rate/channel count, and
stored integer/float format when known. Decoded processing samples are float32; unknown storage
formats remain null. This does not identify ADC or analogue clipping.

`channels` reports zero-based channel indices, sample peak/RMS/DC, `nearSilentSamples`,
`overFullScaleSamples` (absolute value at/above unity), per-channel loudness and headroom. `loudness` contains `integratedLufs`, `momentaryLufs`,
`shortTermLufs`, `loudnessRangeLu` and `truePeakDbtp`. These use FilmCraft's native BS.1770/EBU meter;
per-channel meters treat each channel as a mono program. Integrated loudness needs a gated 400ms
block, and short-term loudness needs three seconds. Non-finite/unavailable decibel readings are
`null`, including silent integrated loudness. Native multichannel speaker weighting uses the
meter's standard channel-count convention; unusual channel roles need an explicit interpretation.

`stereo` reports first-pair correlation, mid/side RMS and mono loss. `M=(L+R)/2`; mono loss is
`-10log10(Mpower / mean(Lpower,Rpower))`. Exact cancellation has `completeMonoCancellation:true`
and `monoLossDb:null`, avoiding a misleading finite bound. Diagnostics do not infer speaker or
musical intent. Low correlation alone does not prove that the sources are unrelated.

`recommendation:{mode:"left"|"right"|null,confidence,reason}` suggests one-sided routing only when
one channel has usable AC signal and the opposite channel is conservatively near-silent. The
confidence is a heuristic label, not a calibrated probability. Mono, dual mono, polarity inversion,
uncorrelated/genuine stereo, silence and insufficient evidence do not trigger automatic downmixing
or inversion. `routing` reports the existing source-channel selection and channel-router effects;
the recommendation describes the declared pre/post boundary. A repair remains a separate edit.
Full-scale sample counts identify samples at/above unity, not proof of irreversible recording clipping.

To hear a preview, decode `audio.base64` (`audio.mimeType:"audio/wav"`, `format:"ieee_float32"`)
using a float-capable player. The WAV is 48 kHz stereo and preserves samples above unity; it is
not normalized, clamped or limited. `headroom` reports the peak, remaining dB and full-scale sample
count. A monitor or player can still clip: reduce monitor gain. Mono sources are copied to both
sides. Native 5.1 uses the BS.775 front/rear stereo downmix, omitting LFE. Unknown multichannel layouts
are refused for preview; explicitly map an isolated clip instead of silently dropping channels.

For frozen proposal comparison, include `subject.plan` and either `snapshot:"before"` /
`"after"` or `compare:true`. Comparison returns `{before,after}` with independently declared
ranges and the frozen plan revision. Subsequent live edits cannot change the captured snapshots.
The subject and explicit range must exist in both snapshots; proposals that remove the clip or
move it outside the requested interval return an actionable error. The live timeline remains untouched.

`audio.inspectAsync` returns `{job,inputRevision,poll:"audio.inspectStatus",cancel:"jobs.cancel"}`.
`audio.inspectStatus {"job":id}` returns `status:"running"|"completed"|"failed"|"cancelled"`,
`inputRevision`, and a `result` report on completion. At most two workers run concurrently; at most
32 bounded session results are retained. Generic durable job IDs, progress, cancellation and
interrupted status survive restart; diagnostic result JSON is session-only and must be measured
again after restart. Results belong to the originating project document and cannot be read through
this endpoint after opening a different document. Background reads use an immutable project
snapshot, native originals and the original host read permissions, with panic/cancellation guards.
