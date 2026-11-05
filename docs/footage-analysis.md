# Native footage analysis, library retrieval and quality checks

All positions use FilmCraft integer ticks (254016000000 per second). The commands use immutable
project snapshots, keep the playhead and undo history, and support both desktop and headless hosts.
An edit proposal is reviewed with `agent.preview` / `render.frames`, then committed with `agent.apply`.

## Measure footage

`analysis.footage` accepts a `subject` with `target: "source"` and `item`, `target: "clip"` and
`clip`/`sequence`, or `target: "sequence"` and an optional sequence ID. Add `plan` to measure the
frozen staged snapshot. `effects: "pre" | "post"` selects the standard effects boundary.

```json
{"subject":{"target":"source","item":42},"start":0,"end":2540160000000,
 "maxFrames":120,"audio":true,"music":true,"waveform":true,"speech":false}
```

A call covers at most 300 seconds and 1200 rendered samples. Use consecutive windows for long media.
`strideFrames: 1` makes a dense frame scan; a larger stride reports a sampling bracket, rather than
claiming an exact cut. `maxFrames` defaults to 120. Frames render at no more than 192 pixels on the
long edge. Results include per-frame luma, black and display-white fractions, mean RGB, saturation,
Laplacian sharpness, motion and existing FilmCraft scene-signature distance. Findings include shot
boundaries, black spans and frozen-or-static spans. Static footage cannot be distinguished from a
held frame solely from identical pixels, so the result states that ambiguity.

Audio is decoded natively at 16 kHz in bounded blocks. Silence uses all-channel 20 ms RMS (default
-42 dBFS); activity is energy, **not speech recognition**. Adjacent-sample jumps are possible clicks,
which may also be intentional transients. The waveform is a generated PNG. Existing remix analysis
provides beat and onset timestamps; footage without a reliable grid returns that status explicitly.
Decode failures or offline media produce an error, never successful silence from an offline slate.

Existing project transcripts supply actual word evidence without a write. `speech: true` runs an
installed local transcriber if no transcript is present; it does not download a model or save the
result to the project. Models remain opt-in through the speech model commands. Every finding and
word carries its requested subject time/frame plus source and timeline mappings. Mapping lists are
capped at 32 and report truncation. Legacy transcript evidence IDs bind the item, source word index,
text and bounds; they are not a claim that text remains identical after transcript replacement.

## Index and retrieve local ranges

`library.index {"items":[42],"start":0,"end":2540160000000,"maxFrames":24}` incrementally indexes
1–32 source items, at most 512 samples per call. An unchanged item/window/sampling density reuses
its entries. Metadata, interpretation, sampled file identity and transcript changes invalidate them.
File identity reads at most the first/last MiB, not a full cryptographic content hash. Explicitly
missing or offline sources cannot be indexed. The local index is scoped to the opened document.

`library.search {"query":"price","limit":10,"thumbnails":true}` returns ranked source ranges,
word evidence, measured descriptors and optional native PNG thumbnails. Literal transcript words,
source names, and measured color/motion/detail descriptors are the built-in fallback. That fallback
does not recognize a crowd, person, action, or wide shot. Stale entries are excluded and identified.

Semantic search uses genuine compatible image and text embeddings. A host can install the public
`library_search::EmbeddingProvider` trait (local image/text inference), then call `library.index`
with `embeddings: true`. No model is bundled or downloaded by this interface. Alternatively an agent
can supply vectors for existing indexed range IDs:

```json
{"model":{"id":"your-local-image-text-model","dimension":512,
          "provenance":"model/version, local inference implementation and image preprocessing"},
 "entries":[{"id":"indexed-range-id","vector":[0.1,0.2]}]}
```

The example vector must be replaced by exactly `dimension` real values. `library.embeddings` rejects
non-finite, zero-norm, mismatched or excessive vectors. Values are normalized to unit length.
Search accepts `embedding: {"model":{...},"vector":[...]}` for the actual text query, or calls the
installed provider. Only identical declared model/dimension/provenance records compare. Results
report real cosine similarity and lexical evidence; absent compatible vectors return explicit
unavailable semantic status. No synthetic vector is generated from numeric descriptors.

`library.selectsPlan {"hits":["range-id"],"name":"Crowd selects"}` creates a staged sequence with
back-to-back approved source ranges. `library.brollPlan {"hits":[...],"sequence":9,"at":0}` stages
silent B-roll on a new video track. Neither applies the edit. Sources are fingerprint-checked again
before proposal. Index capacity is 4096 ranges, 100000 evidence words / 8 MiB text, and 2097152 vector
values. `library.save` and `library.load` take an explicit cache path (version 1; 16 MiB maximum),
validate fingerprints when reopening, and preserve the project. `library.clear {}` clears the index.

## Check a cut

`qc.timeline {"sequence":9,"plan":7,"maxFrames":120}` combines structural coverage with native
sampled analysis. It reports uncovered video gaps, short flash candidates, source-word boundaries
crossed by edits, exact transcript-phrase caption timing drift, native discontinuities at audio edit
edges, offline media, decoded black spans and display-white clipping candidates. Each finding is
located and explains its evidence. Short clips, bright pixels, transients and caption padding can
be intentional. Sparse scans do not certify unseen frames. Translated or unmatched captions report
unverified drift coverage. The scan is bounded to 5000 clip placements and 5000 captions; use smaller
sequences/windows when the comparison budget is exceeded.

## Check the delivery independently

```json
{"path":"/approved/output.mp4","expected":{"duration":2540160000000,
 "frameRate":{"num":60000,"den":1001},"width":1920,"height":1080,
 "colorSpace":"rec709","audio":{"present":true,"sampleRate":48000,"channels":2},
 "captions":{"mode":"sidecar","path":"/approved/output.srt","format":"srt","count":12},
 "toleranceFrames":1}}
```

`qc.export` opens that file with FilmCraft's native media readers; it does not import it, change the
project, invoke ffmpeg, or trust the export job's success receipt. It checks requested duration,
exact rational frame rate, dimensions, color metadata, audio presence/shape, and caption sidecar
format/count/bounds. It actually decodes first/middle/last video frames and short audio windows for
every exposed audio stream, validating sample shape and finite values. `sequence` additionally
compares sidecar text and timing against enabled sequence captions. Tolerance defaults to one frame.
The report describes this sampled coverage and does not certify every encoded sample.

Color metadata inferred by the importer is reported separately and cannot certify delivery tags.
`verified` is true only if every requested check passes. Burned-in text recognition and embedded
subtitle/closed-caption demux are not exposed by this verifier: requesting those modes, or proving
no embedded captions, returns `unknown` and `verified: false`. Sidecar caption delivery is verified
with native parsers. This boundary avoids a false pass when a container carries streams the media
reader does not expose.
