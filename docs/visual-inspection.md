# Inspecting exact frames and color measurements

`render.frames` and `scopes.read` render through the CPU reference pipeline from an immutable
project snapshot. They do not seek the transport or capture a monitor window. These engine commands
work through `command_run`, the CLI, and the desktop control channel using the same implementation.
They always read original media, independent of the monitor's proxy setting. Missing or unreadable
sources are reported in each frame's `offlineMedia`; an offline slate is not a successful media check.

## Exact visual previews

```json
{"target":"sequence","sequence":12,"frames":[0,48,96],"maxSide":960,"layout":"contactSheet"}
```

Use `target:"clip"` with a timeline `clip` and optional `sequence` to isolate a clip, or
`target:"source"` with a project `item` to inspect source media. Source positions are **absolute
media positions**, including for subclips. Timeline clip positions are **sequence positions**.
An isolated clip includes its intrinsic placement, opacity, masks, and standard effects, with no
other clips, transitions, or sequence captions composited into the picture. The full snapshot
remains available to effects that explicitly sample another track. Adjustment layers require a
sequence composite because they have no isolated picture of their own.

Choose exactly one position form: `frames:[integer]`, `times:[ticks]`, `frame`, `time`, `seconds`, or
`timecode`. Omission uses the sequence's playhead, the isolated clip's start, or the source's start.
Negative, unrepresentable, or out-of-range source/clip positions fail. A time is snapped down to its
subject's frame boundary; the response reports the actual `time` and `frame`.

The default `layout:"stills"` returns one `image` per frame. Each image contains `mimeType`,
`width`, `height`, and PNG `base64`. A `contactSheet` instead contains the timecoded sheet and a
`cells` index mapping its labeled rectangles to the returned frame records. Every record identifies
its sequence, clip, requested source frame/time where applicable, revision, frame rate, pixel aspect,
effect boundary, and color transform. Source mapping is the requested media position; decoder PTS
for variable-frame-rate media is not separately reported. PNGs are sRGB RGBA8 over black, with FilmCraft's SDR monitor
transform applied; they are visual previews, not HDR deliverables.

`maxSide` defaults to 960. Zero requests full resolution. At most 24 images, 8192 pixels per side,
and 32 megapixels of total frame output are allowed. This permits one full 7680×4320 still;
use reduced previews for large batches. Contact sheets have a separate 32-megapixel canvas limit.
Encoded PNG responses are capped at 48 MiB before base64 expansion. These limits produce errors
with guidance to reduce resolution or frame count.

A `revision` constraint rejects a request if its snapshot is different. For staged edits, pass
`plan` to render the proposed snapshot, or `plan` plus `comparison:true` for alternating before and
after frames. Both sides retain the plan's base revision, and `snapshot` identifies `before` or
`staged`; they can be inspected while the live timeline changes. Applying stale plans is separate
and is refused by the plan workflow.

## Subject-specific scopes

```json
{
  "target":"clip","sequence":12,"clip":48,"frames":[24,48,72],"effects":"post",
  "roi":{"x":0.3,"y":0.1,"width":0.25,"height":0.4},
  "scopes":["waveform","histogram","vectorscopeYuv"],"bins":false,"images":true,
  "reference":{"target":"source","item":20,"frame":36,
    "roi":{"x":0.25,"y":0.1,"width":0.3,"height":0.4}}
}
```

The subject and position fields match `render.frames`. `effects:"pre"` bypasses standard effects;
intrinsic placement, opacity, and opacity masks remain. `post` is the default. The normalized ROI
must have positive area and fit inside the 0–1 subject canvas. It is cropped **before** scope
sampling, so a small face region retains local samples rather than cropping an already reduced
whole-frame scope. Signals use at most 480×270 nearest samples of that ROI.

Existing scope options remain: `scopes`, `waveformType`, `paradeType`, `colorSpace`, `clamp`,
`columns`, `peaks`, and `bins`. Use `maxSide` or the older `scale` field to control render resolution,
never both. `images:true` adds PNG plots to the numeric scope results. Plots contain traces without
UI chrome; the response supplies their matrix, range and units.

`colorSpace:"auto"` chooses Rec. 709 analysis for SDR or BT.2020/PQ for an HDR subject. Explicit
`2100` renders working light, transforms its working gamut to BT.2020, and measures PQ code values
(0–100%, 0–10000 cd/m²). SDR results use the exact 8-bit sRGB monitor signal; `601` selects the
BT.601 YCbCr analysis matrix on that monitor signal, rather than claiming to convert the source
primaries. The returned `signal`, `matrix`, `unit`, and transform identify these choices.

`clipping` reports per-channel counts and percentages at black and white code, samples outside the
range before quantization, non-finite samples, and (for PQ) pixels above the 203 cd/m² reference
white. These are sampled measurements of the declared ROI, not a claim about unsampled pixels or
the eventual export. Optional `reference` accepts a second subject, one position and its own ROI.
Each frame receives `referenceDelta` in percentage points, **subject minus reference**.

For compatibility, a single position returns the reading object directly. `frames` or `times`
returns `{revision,plan,frames:[reading],reference}`. All readings identify their exact subject,
position, snapshot revision, declared signal, and offline sources.
