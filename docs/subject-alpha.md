# Reviewable subject alpha and framing variants

Subject masks store actual source-pixel alpha keyframes in the project. They use the same effect
mask feather, expansion, opacity, invert and combination controls as drawn paths. They survive
save/reopen, render headlessly, participate in frozen plan previews, and apply as one undo step.

## Select and track

```json
{"clip":17,"sequence":9,"effectInstance":"opacity:0",
 "prompt":{"rect":[0.2,0.1,0.3,0.7]},"start":0,"end":1270080000000,
 "maxFrames":120,"strideFrames":1,"maxSide":128,"minimumConfidence":0.35}
```

`subject.trackPlan` returns a staged `agent.plan`, plus `subjectTracking.readings` containing actual
source/timeline positions, confidence, failed/reacquired state, alpha centroid and visible fraction.
`failedRanges` makes losses reviewable. Prompts use normalized coordinates on the **source** image,
not the transformed Program canvas. Choose exactly one `rect: [x,y,width,height]` or `point: [x,y]`.
Optional `foreground` and `background` point arrays supply color-model correction samples.
`effectInstance` is the effect name and ordinal on that clip, revision-scoped like grade addressing.
It defaults to the intrinsic `opacity:0`; another video effect limits that effect instead of cutting
out the whole layer. `mask` updates an existing index only with explicit `replace: true`.

The built-in implementation is original, local color segmentation: foreground/background color
prototypes, an edge-weighted Potts graph regularizer, connected components, and a motion/area prior.
It traces pixels and preserves holes instead of moving an ellipse or rectangle. A lost subject emits
a transparent failed key; the retained appearance model can reacquire after occlusion. It does not
know that a region is a person. Similar colors, camouflage, lookalikes, large lighting changes or
complex occlusion can defeat it or produce a wrong confident component. Review the frames and
correct the mask. Confidence is an algorithmic score, not a calibrated probability of identity.

Tracking renders ungraded original source frames at 32–256 pixels on the long edge (default 128),
with at most 240 samples per request. Source range and dimensions are validated before a plan is
created. Requests beyond the frame budget must shorten the range or explicitly increase
`strideFrames`; they are not silently truncated. Alpha holds the preceding sampled raster between
keys. Use dense stride 1 for every displayed frame. Before/after output uses the shared
`render.frames {"plan":id,"comparison":true,...}` renderer. Nothing lands before `agent.apply`.

## Inspect and correct

`subject.inspect {"clip":17,"mask":0,"mediaTime":0}` returns the method/provenance, per-key
confidence, failed/corrected state and centroid/area, plus an optional PNG of the actual alpha.
`includeData: true` returns the typed raster track. Inspecting a locked track is allowed.

`subject.correctPlan {"clip":17,"mask":0,"mediaTime":0,"prompt":{"point":[0.4,0.5]}}`
resegments that source frame with the corrected prompt and proposes replacement of that one key.
Other keys remain. `mediaTime` is source ticks; `start`/`end` on tracking are sequence ticks. A
correction that cannot separate foreground from background returns an actionable error.
Updating a tracked range with `subject.trackPlan`, an existing `mask`, and `replace: true` retains
keys outside that range. All changes still require the plan's original project revision.

## Build an editable aspect-ratio variant

`subject.reframePlan {"sequence":9,"clips":[17],"width":1080,"height":1920}` duplicates the full
sequence, preserving grades, speed changes, transitions, linked audio, captions and mixer settings.
It adds editable Motion scale/position keys from confident alpha centroids, fills the requested
square-pixel canvas, and keeps the source in frame at its edges. The original sequence stays intact.
Both 16:9 and 9:16 targets use this same command with the desired dimensions.

The tracking alpha is retained but disabled in the framing variant so the original background
remains visible. Existing Motion animation requires explicit `replaceMotion: true`. Low-confidence
and failed alpha keys are skipped; Motion interpolates between the remaining confident keys. Those
intervals still need visual review. This is editable deterministic framing from measured alpha,
not a claim that an occluded subject's hidden location is known.

## Storage and rendering bounds

`Mask.alpha` is optional and serde-defaulted for existing projects. `AlphaTrack` records source
size, exact media-time end, method/provenance and ordered `AlphaFrame` keys. Each raster has at most
256×256 / 65536 pixels. Packed RLE runs store `(length << 8) | alpha`; zero runs, corrupt sums,
non-finite confidence, duplicate/out-of-range times and excessive allocations are rejected. A
track holds at most 1200 keys, 8 Mi raster pixels and 1 Mi packed runs. The plan protocol's request
budget may impose a smaller practical window; reduce sample size/count when it does.

CPU rendering builds a bounded signed-distance field from the raster, preserving holes and using
the ordinary mask controls. GPU callers use the identical CPU coverage/mix fallback for raster
masks; geometric masks retain their existing shader. Outside the reviewed alpha interval, and on
failed keys, the raster is transparent. A native model may be added separately in future; no model
weights, downloads or external executables are used by this implementation.
