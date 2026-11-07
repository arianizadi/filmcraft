# Gallery, grade versions, shared looks and color interchange

Color workflow commands use explicit clip IDs and `expectedRevision` from inspection. They work in
headless sessions and staged edit plans. Stills, versions and shared looks are saved inside the
project and survive reopening. A master look renders before the clip's local effects; local color
controls remain editable per-shot trims. Splitting or copying a clip preserves its shared look.
Updating a master refreshes the rendered appearance and preview hashes of every linked clip.

Save an isolated clip's exact displayed frame and grade:

```json
{"id":"gallery.save","params":{"name":"Hero face","target":"clip","clip":12,"frame":48,"maxSide":1920,"expectedRevision":7}}
```

`gallery.get` returns the frozen PNG, frame, sequence/source IDs, captured revision and pipeline.
The same frame also carries a bounded frozen working-linear sample for matching; changing or
relinking the original footage does not change this reference. `gallery.list`, `gallery.delete` and
`gallery.applyGrade` manage stills. Source/composite stills are visual references; an applicable grade
comes from an isolated timeline clip. Display stills include the declared display transform; the
matching sample retains working-linear pixels. PNGs and samples are project data, with a total
64 MiB gallery limit. Capture a full-resolution still by setting `maxSide` to the source frame's
longest side, subject to the normal frame-render allocation limits.

Use a saved reference in `grade.matchBatch`:

```json
{"id":"grade.matchBatch","params":{"reference":{"gallery":"Hero face"},"targets":[{"clip":18,"effectInstance":"new"},{"clip":22,"effectInstance":"new"}],"mode":"exposure","expectedRevision":8}}
```

An imported source item or a timeline hero clip remains available through `reference.item` or
`reference.clip`. Matching reports confidence, per-shot before/after measurements and verified
improvement. Frame previews use `render.frames` against the before/after plan snapshots. Current
matching requires SDR Rec.709 working sequences; HDR source footage can be color-managed into an SDR
sequence. An HDR/wide-gamut working sequence is rejected before editing.

`grade.version.save` stores a named grade for the addressed clip. `grade.version.list`,
`grade.version.apply` and `grade.version.delete` manage alternatives. A version preserves the color
stack's positions around non-color effects and snapshots the effective shared grade, so switching
versions is independent of later master changes. Applying a version detaches the shared look. If the
non-color effect structure has changed, application requires explicit `allowStructureChange:true`;
existing color animation requires `replaceKeyframes:true`.

`grade.look.save` captures the source clip's local color stack. Name and group identify a camera or
scene look. `grade.look.link` assigns it to a clip, `grade.look.update` changes the master from another
clip, and `grade.look.list` exposes its controls and linked clip IDs. Shared looks are static and
unmasked; `freezeAnimation:true` plus an exact frame captures an animated look. Keep masks and
animation in local trims. A master cannot update a clip on a locked track. `grade.look.unlink` defaults
to `keepAppearance:true`, inserting the frozen master before local effects. Delete a master only
after unlinking its clips. Pipeline changes and replacing an existing link require explicit consent.

ASC CDL commands interchange the forward/clamped SOP and saturation form defined by ASC CDL v1.2
and the Academy CLF specification. `grade.cdl.inspect` accepts inline `xml` or a `.cc`, `.ccc`, `.cdl`
file `path`. `grade.cdl.import` selects a correction by ID and an explicit `asc_cdl:N` effect instance
or `new`. FilmCraft's ASC CDL effect operates in sRGB-encoded Rec.709, which the commands declare;
import/export requires an SDR Rec.709 working sequence. Values outside the editable engine ranges
are rejected without clamping. `grade.cdl.export` exports the selected CDL effect and reports other
omitted effects and geometric masks; it does not approximate a full Lumetri grade as a CDL. Animated
export requires `sampleAnimation:true` and an exact frame.

`grade.bakeLut` samples the actual supported color stack, including shared grades and library LUTs,
into a `.cube`. Its default 33³ grid and optional 2…65 size use tetrahedral interpolation. Defaults
are working-linear RGB and domain 0…1. `encoding:"srgb"` is available for SDR Rec.709; HDR bakes use
working-linear units (1.0 = 203 cd/m²), with a domain large enough for the desired highlight range.
The output records working gamut, pipeline, encoding, domain, source frame and revision. A sampled
3D LUT approximates values between grid points; it cannot reproduce spatial or temporal processing.

Every effect receives an inclusion/omission reason. Vignettes, sharpening, geometric masks, HSL
mask denoise/blur, spatial effects and unsampled animation are not representable. A request needing
omissions returns a report and writes no file until `allowPartial:true` is explicitly supplied; that
choice omits each unsupported effect in full. Intrinsic placement/opacity controls are always listed
outside the color bake. `sampleAnimation:true` captures controls at the selected frame, not animation.

CDL export and LUT baking return text by default. An optional output `path` publishes a complete new
file atomically and refuses an occupied destination. File outputs require Edit permission and stay
outside staged plans. In-memory library edits and CDL import can be staged, previewed and applied
as one revision-guarded undo step.

## GPU LUT grading

Export with `settings.gpuRendering:"auto"` to use the offscreen GPU compositor. SDR Lumetri input
and creative LUTs now run in the effect stage, including 1D shapers, independent input domains,
and exact tetrahedral 3D interpolation. Immutable tables remain uploaded across frames; creative
looks retain their position before wheels, curves and HSL correction. The desktop app's
`perf.stats.gpuEffects` reports actual LUT compute dispatches and texture uploads through its
control channel or MCP bridge.

The GPU table budget is 1,048,576 combined shaper/cube entries (16 MiB before curve tables),
which covers 65³ camera LUTs. Larger tables, device texture limits, nonportable numerical domains,
HDR grading and unsupported spatial effects retain the CPU renderer. GPU rendering remains an
explicit export choice; hardware decoding and encoding are independent settings.
