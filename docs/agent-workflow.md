# Reviewable agent editing

FilmCraft exposes an **inspect → propose → preview → apply → verify** workflow through the
desktop app, CLI and MCP. An external agent, such as Claude or Codex, chooses edits from your
instructions and footage. FilmCraft stages and renders those edits so you can approve the
result before it reaches the live timeline.

The native Agent Review window also prepares edits directly from source paths: choose Talking head,
DJ / music, or Selects, prepare a proposal, inspect the diff and frames, approve it, then export and
verify. Local analysis and installed speech models provide evidence; creative decisions remain
reviewable. See [wishlist progress](wishlist-progress.md) for implementation and validation evidence.

## Start the Mac app and connect an agent

Use the desktop app and CLI built from the same checkout/release. Python 3 is required for the
helper. From the repository:

```sh
python3 scripts/start-agent-workflow.py start
python3 scripts/start-agent-workflow.py config
```

The helper discovers local release builds and the packaged app. For an installed app or another
build location, provide explicit paths on both calls:

```sh
python3 scripts/start-agent-workflow.py start \
  --app /Applications/FilmCraft.app \
  --cli /absolute/path/to/filmcraft-cli

python3 scripts/start-agent-workflow.py config \
  --app /Applications/FilmCraft.app \
  --cli /absolute/path/to/filmcraft-cli
```

The second command prints an MCP server stanza that you can add to your agent's MCP settings.
It contains absolute executable paths and no token. The wrapper reads the token when MCP starts;
no account configuration is changed by this script. Keep the helper at the path in the stanza.

The helper stores a persistent 64-character random token in
`~/Library/Application Support/FilmCraft/Agent/control-token` with mode `0600`, inside a `0700`
directory. It never prints the token. The app and CLI receive it only through their environment.
The helper refuses occupied ports and checks the authenticated app session before connecting
MCP, so it cannot silently attach to another app process. Use the same `--port` on every call
when choosing a port other than `9876`.

```sh
python3 scripts/start-agent-workflow.py status
```

`status` shows the authenticated app session, revision and permission. Startup errors name
`desktop.log` in the support directory. A startup timeout leaves the app open; save and close
that helper-launched instance before retrying. Optional `--project /absolute/project.fcproj`
opens an existing project when starting.

## Permissions and approval

The desktop app started with `--control` begins in **Propose** permission. Open
**Window → Agent Review** to inspect proposals and change permission yourself.

| Permission | Agent access |
| --- | --- |
| Inspect | Read the project, typed grades and scopes; render frames. |
| Propose | Inspect and prepare/discard staged edit plans. You approve timeline changes in the app. |
| Edit | Apply plans and run direct edits, file writes and jobs. |

Generic commands and synthetic UI actions obey these permissions too. Paired connections additionally
use persistent per-client grants for one saved project and approved media/output folders. The
Connections section accepts or revokes requests; paired clients use scoped engine rendering, while
window screenshots, window inspection and synthetic input require the owner connection. A command's exclusion
from the undo journal does not make it a read: file exports, downloads, jobs and live controls
still require Edit. Permissions are session state; they are selected again when the app restarts.

Import footage through the app, grant Edit for direct import, or use `workflow.prepare` to stage
source metadata and proposed edits together. Model downloads, saves and export jobs happen outside
edit plans. Return to Propose for
reviewing timeline edits. Agents can prepare a new proposal while you keep editing; any live
project revision change makes the old proposal stale and prevents applying it.

## The agent's editing loop

1. Call `agent_session` and retain its `session_id` and revision. Inspect the project/sequence
   with `inspect`, `project_inspect`, `sequence_inspect` or `doc_inspect`. Use `command_list`
   categories/filter and `command_describe` to find commands and their enabled/disabled reason.
2. Call `plan_edit` with a descriptive label, `base_revision`, and eligible engine commands.
   Simulation runs on an isolated snapshot. The response includes the plan ID, per-step
   results, stable created clip IDs, typed before/after changes and warnings.
3. Read `preview_plan`. Check locked tracks, linked clips, transitions and captions in its diff.
   Call `render_frames` with the plan and `comparison:true`; compare isolated clips or the full
   sequence. Read `scopes_read` for the same frames and subject, optionally with a normalized ROI.
4. Approve in **Agent Review**, or explicitly allow the agent to call `apply_plan` with Edit
   permission. Applying the exact staged snapshot creates one undo step. A stale plan refuses
   to apply; rebuild and preview it against the new revision.
5. Verify the receipt, actual affected IDs and resulting revision. Re-read the grade/scopes and
   render the applied result. Save the project through an explicit save operation when ready.

`command_batch` retains its legacy sequential behavior and can leave earlier successful steps
applied if a later step fails. Use `plan_edit` for atomic timeline edits. The staged allowlist
rejects unsupported commands, filesystem writes, jobs and arbitrary UI actions.

Plan parameters may reference an earlier step's result:

```json
{"clip":{"$step":0,"path":"/clip"}}
```

The JSON Pointer path must name the actual returned field. Inspect the creation command's
result rather than guessing an ID. Grade commands inside a plan receive the sandbox's current
`expectedRevision` automatically when that field is omitted.

## Match camera B to a hero shot

Get the real sequence and clip IDs from inspection. This example uses hero clip `101` and
camera-B clip `202`; replace them with your IDs. The `plan_edit` tool uses snake_case for its
own arguments and engine camelCase inside each step's `params`:

```json
{
  "label":"Match camera B to camera A",
  "base_revision":41,
  "steps":[
    {
      "id":"grade.matchBatch",
      "params":{
        "reference":{"clip":101},
        "targets":[{"clip":202,"effectInstance":"new"}],
        "mode":"full",
        "samplesPerClip":3,
        "minConfidence":0.25,
        "skinProtection":true
      }
    }
  ]
}
```

The hero can be an isolated timeline clip in another sequence (`sequence` + `clip`) or an
already-imported source still/video (`item`). Each shot samples one to eight frames; explicit
`frames` select exact sequence-frame indexes, or source-frame indexes for an `item` reference.
Targets use the persistent `effect:<id>` address returned by `grade_get`, or `"new"`.
Legacy ordinal addresses remain compatible with revision checks; see [stable effect identities](effect-identities.md).

Modes are `full`, `exposure` and `white_balance`. The latter two preserve controls outside their
requested pass. Low-confidence shots and candidates without verified numerical improvement
are skipped. Confidence is a documented heuristic of tonal overlap and unclipped coverage,
not a calibrated probability. The result reports the sampled times, before/after Oklab tonal
statistics and distance deltas for every shot. Rendering the staged plan shows the actual image.

Example `render_frames` MCP arguments:

```json
{"target":"clip","clip":202,"frames":[48,72,96],"plan":7,"comparison":true,"max_side":960}
```

Use the real plan ID and frames inside that clip. `target:"sequence"` renders its composite;
`target:"source", item:...` renders original media. `contact_sheet:true` adds timecoded labels.
Metadata names the actual frame, timecode, revision and color transform. Rendering uses fixed
snapshots in both desktop and headless mode and leaves the playhead alone.

For two approved dialogue removals, include the supported transcript/extract commands in the
same plan as the match. Inspect the transcript and proposed linked-track changes first. When
using absolute timeline ranges, process the later removal first so earlier coordinates remain
valid; verify the resulting dialogue and captions with previews before approval.

## Typed grades and scopes

`grade_get` returns complete Lumetri parameter values, wheel vectors, curves, LUT references,
masks and keyframes. Addresses such as `lumetri:0` identify the ordinal Lumetri instance on that
clip at the returned revision; they are not persistent effect UUIDs. Direct grade writes require
`expected_revision` so an intervening edit cannot retarget a stale address.

A partial `grade.patch` changes only supplied fields:

```json
{
  "clip":202,
  "effectInstance":"lumetri:0",
  "expectedRevision":42,
  "params":{"exposure":{"value":{"Float":0.5}}}
}
```

This is an engine command payload. The typed MCP `grade_patch` uses `effect_instance` and
`expected_revision` at its top level, with the same `params` contents. Omitted controls,
masks and animation are preserved. Providing replacement keyframes for an already-animated
control requires `replaceKeyframes:true` (MCP `replace_keyframes:true`). `grade.copy` and
`grade.reset` enforce equivalent guards. Keyframe times use **source-media ticks**, matching
FilmCraft's effect renderer; `254016000000` ticks equals one second.

`lumetri.applyMatch` validates the reference before adding a missing effect and requires an
explicit `replaceKeyframes:true` before replacing affected animation. Adding the effect and
applying its match share one undo step.

`scopes_read` supports source media, an isolated clip or a sequence composite, pre/post-effects
sampling, multiple exact frames, normalized `roi:{x,y,width,height}`, reference deltas, clipping
metrics and optional scope images. Check the declared color space/transform and `offlineMedia`
metadata before treating a measurement as footage evidence.

## Lost replies and retries

For writes, use `operation_run`, or `apply_plan` for an approved proposal. Retain the exact
`operation_id`, `idempotency_key`, `session_id` and arguments across retries. Operation receipts
include commit status, affected IDs and before/after revisions. Start exports in operation envelopes
with `wait:false`, then poll the returned job ID with `jobs.list`; blocking exports are rejected
there. The existing `command_run` export path also supports progress/cancellation with `wait:true`. A lost bridge reply retries the
same operation and returns its receipt instead of performing the edit again.

Call `get_operation` after an uncertain response. `unknown_commit` means the result is uncertain;
inspect the project before deciding to create a new operation. It never proves the edit failed.
After app restart, an old session ID refuses execution instead of replaying the write in a new
session. Receipts and staged plans currently live for the app session; save the project before
closing it. Direct UI gestures are not automatically retried.

## Local speech recognition

Whisper and local CLIP inference are enabled in standard native app/CLI builds. Models still require
an explicit opt-in download. A normal build is:

```sh
cargo build --release -p filmcraft -p filmcraft-cli
```

Inspect `transcript.models` and command enablement in the running build. Download a selected
model explicitly with `transcript.downloadModel` using Edit permission; its durable job supports
progress/cancellation through `jobs.get` / `jobs.cancel`. Models are not bundled
and no download starts from this launcher. Existing transcript generation, caption creation,
filler removal and pause removal can then support the agent's proposed edits. See
[transcripts.md](transcripts.md) for model sizes, licenses and commands.

## Current limits

- Automatic matching supports SDR Rec.709 **working sequences**. HDR/wide-gamut working-space
  matching is explicitly rejected before mutation because the current numerical solver evaluates
  SDR grading math. Imported HDR/log footage may be color-managed into an SDR sequence, but
  validate its transform and image before matching. HDR scopes/rendering remain separate tools.
- Staged plans and operation receipts live in one app session. Saved checkpoints, annotations,
  activity, grade versions and profiles live in the project. Durable job receipts survive restart;
  unfinished jobs become interrupted and never resume silently.
- Automatic selection uses measured samples, transcript pauses/fillers and optional music beats.
  It cannot guarantee a creatively finished film from arbitrary footage. Review proposed ranges.
  Semantic retrieval uses local CLIP when loaded; sampled frames can miss events and scores are
  not calibrated confidence probabilities. See [local search](local-vision.md).
- Subject tracking is seeded classical color/graph alpha tracking, with confidence, failed ranges
  and explicit correction frames. It can lose similar-colored, heavily occluded or fast subjects;
  failed ranges are exposed for correction. It is not a neural person/object segmentation model.
- Motion templates use bounded native editable graphics and keyframes. Stem jobs offer harmonic /
  percussive or mid/side separation, not learned vocal/instrument isolation. Upscaling resamples
  pixels without inventing detail. HDR-to-SDR upscale requires explicit `allowSdr:true`.
- Cut QC is evidence-based and sampled; it does not certify unseen frames. Export QC reopens and
  decodes the output, checks timing, geometry, color and audio, and can compare complete sidecar
  captions. Burned/embedded captions and complete embedded-caption absence currently return
  unknown rather than certification. Translation accepts supplied translated cues with exact timing.
- Typed MCP schemas cover the primary review/render/grade/scope tools. Advanced legacy commands
  retain their parameter documentation through `command_describe` and `command_run`.

Use the reviewed plan workflow on copies of your projects while assessing it on real footage.
Synthetic regression tests establish transaction/retry behavior and numerical matching; they
do not establish creative quality or compatibility with every camera file.

## Fork updates

This fork uses its own version suffix and `arianizadi/filmcraft` release source. Startup update
prompts are disabled until fork releases are published. Help > Check for Updates checks only
the fork; it cannot replace the custom build with a `storytold/filmcraft` release. The current fork includes upstream main through `ba8d04a3`; compare commit ancestry before
merging later upstream changes.

## 8K and custom frame rates

Sequences accept positive rational rates with unsigned 32-bit components and at least one
FilmCraft tick per frame. Use `frameRate:{"num":960,"den":1}` for 1080p960 or
`frameRate:{"num":60000,"den":1001}` for 8K59.94. Sequence Settings includes a custom
numerator/denominator control. Rational boundaries remain exact through inspection and plans.

Encoder/container limits still apply. The H.264 encoder rejects pictures/rates beyond its
implemented levels and rejects timing that its VUI cannot represent exactly. Configuration
regressions cover 8K60, 1080p960 and 1080p2000; 4K960 exceeds those H.264 levels. Native
8K59.94 and 1080p960 import/render/export samples were also exercised locally. These short
tests establish file/timing behavior, not sustained playback or encoding speed.

See [format-compatibility.md](format-compatibility.md) for the actual container/codec matrix
and the reproducible fixture audit. A recognized extension is not a guarantee for every
codec, profile, bit depth, substream or encryption scheme it could contain.

## Automatic preparation and local jobs

The typed MCP `prepare_footage` tool starts background `workflow.prepareAsync` and returns a stable
job ID. Poll `prepare_status` (`workflow.status`) until it reports `staged` and a plan ID. Render and
review that plan before approving. Continued editing can make the proposal stale; adoption and apply
both check the exact baseline. `jobs.cancel` stops preparation without importing staged source
metadata into the live project. Synchronous `workflow.prepare` remains available for advanced scripts.

The native review window offers the same preparation controls, local model opt-in, progress and
cancellation. Approved delivery uses a retained source snapshot for later QC even when you edit or
switch projects while the export runs. Typed `deliver_edit` uses explicit session/operation/key and
revision fields for safe retry. Delivery rejects occupied output paths and profile geometry/rate/color
changes that require a fresh reviewed plan.

See [color versions and looks](color-workflow.md), [analysis and QC](footage-analysis.md),
[local semantic search](local-vision.md), [subject alpha and reframing](subject-alpha.md), and
[audio, upscale and motion jobs](local-media-jobs.md) for the complete command examples and limits.
