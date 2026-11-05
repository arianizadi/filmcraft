# Personal editing wishlist implementation

This tracks the requested wishlist against executable behavior and review evidence. A feature is checked only after implementation, integration and validation. The foundation shipped in `66ce2e8f`; the expanded implementation is `0.17.1-arian.2`, including upstream main through `ba8d04a3`. Explicit algorithm, verification and legacy API limits are recorded below and in the linked guides.

## Safety and agent control

- [x] Operation identity, same-key retry protection, affected identities and revision receipts; ambiguous commits queryable.
- [x] Isolated staged edit plans, exact-revision apply, one undo step, discard and before/after diffs.
- [x] Snapshot frame rendering for sources, isolated clips, composites and staged plans with timecodes/color metadata.
- [x] Typed partial grade edits and guarded matching that preserves animation.
- [x] Subject/ROI/pre-post/multiframe scopes with deltas and clipping measurements.
- [x] Targeted inspection, searchable categories, command descriptions, stable created identities and plan step references.
- [x] Persistent effect-instance identities, deterministic migration and non-reuse after undo/save; typed primary MCP tools and automatic preparation/delivery/search/analysis schemas. Advanced legacy commands retain documented parameters.

## Grading, analysis, editing and delivery

- [x] Multi-sample shot matching against timeline or imported reference, selected passes, heuristic confidence skips and verified deltas (SDR working spaces).
- [x] Named still gallery, grade versions, shared camera/scene looks and per-shot trims.
- [x] ASC CDL interchange and supported-stack LUT baking with explicit exclusions/domain/color space.
- [x] Footage shot/black/freeze/motion/sharpness/speech/silence analysis, waveform images and music timestamps.
- [x] Incremental local library index, ranked source ranges, visual/transcript evidence and semantic embedding support.
- [x] Search results to selects and B-roll proposals.
- [x] Speech-enabled Mac release build with explicit model download.
- [x] Speech enabled in standard native builds; stable source-word cut proposals, per-range review, linked-track diffs and translated cue tracks.
- [x] Saved checkpoints, annotations, activity history, checkpoint timeline diffs and partial proposal approval.
- [x] Timecoded cut QC and independent native exported-file verification against delivery settings.
- [x] Versioned editing profiles and profile-driven PiP/split-screen/vertical plans.
- [x] Individual client pairing and project-scoped media/output folder grants across generic/UI routes.
- [x] Durable job IDs, input revision/progress/cancellation and interrupted-after-restart status.
- [x] One entry point to prepare footage, propose an automatic edit and verify its delivery using the above tools.

## Engine milestones

- [x] Prompted subject alpha masks, occlusion/reacquisition confidence, failed ranges and correction keyframes.
- [x] Subject-driven editable landscape/portrait reframing.
- [x] Parameterized sandboxed motion templates.
- [x] Dialogue enhancement, local harmonic/percussive and mid/side stem jobs with previews/provenance, and optional native resampling/upscale jobs.
- [x] GPU curves, wheels and HSL with CPU-reference comparisons.
- [x] VideoToolbox HEVC Main 10 and independently verified PQ/HLG HDR delivery.

## Resolution, rates and formats

- [x] Exact positive rational frame rates without a fixed 1000fps application cap; explicit codec/representability errors.
- [x] Real 8K59.94 import/render and short export; generated 1080p960 roundtrip. These are correctness samples, not sustained throughput promises.
- [x] Native format audit: 41 verified input profiles and 20 verified export types on the development Mac; see `format-compatibility.md` for named exceptions.
- [x] Original desktop footage preserved; all test edits and exports are separate.

## Validation for the initial foundation

Full `cargo xtask ci` passed with 4,774 successful test executions, formatting, all-target workspace Clippy, layers, assets and 49-crate WASM checking. The installed app and actual MCP bridge were exercised with an unapplied proposal. Subsequent milestones must rerun the required gates before commit/push and refresh the installed build.

## Validation for the expanded workflow

`RUST_TEST_THREADS=4 cargo xtask ci` passed: 4,946 successful test executions, formatting,
all-target workspace Clippy, dependency layers, all 61 asset attributions and WASM checking for
50 crates. Four test workers bound simultaneous native UI/background workers on the development
Mac; no test cases were removed. Independent focused reviews found and fixed source overwrite,
folder/receipt access, delivery snapshot, caption verification, retry, stable identity and job
recovery problems before publication. Added dependencies were checked for permitted licenses.
Explicit screenshot outputs also create files exclusively and refuse existing files, hardlinks
and symlinks; inline screenshots remain available without writing a file.

Actual local footage validation covered 7680×4320 HEVC Main 10 HLG at 60000/1001, native stills,
a two-frame full-resolution Main 10 export and native/ffmpeg reopening, with unchanged full source
SHA-256. That export took 7.09 seconds; sustained 8K throughput is untested. Short PQ/HLG exports
matched ProRes HDR references at 57.2–58.2 dB luma PSNR, with correct tags and actual ten-bit values.
Generated 1080p960 timing and the previously recorded 41-input/20-output format audit remain covered.

Both approved local models are installed with pinned SHA-256 and license/attribution records:
CLIP ViT-B/32 (about608MB) and float32 Whisper base (about293MB). Real visual queries ranked the
laptop/desk scene above unrelated forest/crowd descriptions. The automatic entry point recognized
85 words, prepared an isolated proposal in28.4s, applied as one undo step, exported a short edit,
passed native QC and replayed delivery with the same job. Footage never left the Mac. Reports,
images, projects and commands live in the local Desktop `FilmCraft Tests` folder; private footage
and derived images are not committed.

## Explicit limits

- Automatic selection is a deterministic measured/transcript heuristic; agents supply creative
  judgment. Local CLIP scores are ranked similarities, not calibrated confidence probabilities.
- Subject alpha uses classical seeded color/graph tracking, with visible failure ranges and
  correction frames. It is not neural semantic segmentation and can fail under occlusion or
  similar foreground/background colors.
- Audio separation offers harmonic/percussive or mid/side derivatives. Upscaling resamples pixels.
  Learned vocal/instrument isolation and generated detail/media models are not bundled.
- Caption translation accepts supplied translated cues and preserves timing. Native delivery QC
  verifies complete sidecar content/timing. Burned/embedded captions and complete embedded-caption
  absence remain unknown; automatic sidecar delivery requires one enabled caption language.
- Automatic delivery supports single-file outputs. Image sequences and MXF OP-Atom remain available
  through explicit export commands. HDR matching still requires an SDR working sequence, and a LUT
  bake reports effects/masks/animation it cannot represent.
- Typed schemas cover the primary workflow tools. Advanced legacy command parameters remain
  documentation-based. Container/codec/profile and exact-time limits are reported explicitly;
  this is not support for every possible media encoding or real-time playback at every rate.
