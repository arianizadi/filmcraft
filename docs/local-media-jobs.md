# Local media jobs and editable motion

Commands are available through `command_run`. Output jobs require Edit permission and run outside
edit plans. They return stable `job` IDs; inspect `jobs.get`, cancel with `jobs.cancel`, and retain
provenance with derivatives. Sources and project history are preserved. Native source protection
also rejects aliases, proxies, numbered source frames and project files as output targets.

## Audio derivatives

```json
{"id":"audio.dialogueEnhance","params":{"item":42,"path":"/approved/output/dialogue.wav"}}
{"id":"audio.separateStems","params":{"item":42,"directory":"/approved/output/stems","method":"hpss"}}
```

Dialogue processing uses FilmCraft's local denoise, speech enhancement and compression. Separation
supports harmonic/percussive (`hpss`) and center/side (`mid_side`) analysis. These are deterministic
DSP algorithms, not learned vocal, drum, bass or instrument isolation. Preview the actual outputs
before placing them on the timeline. The package includes short WAV previews and JSON provenance.

`start` and `end` are source ticks and must stay inside the selected media/subclip. Streaming
processing limits decoded block memory; jobs accept rates from 8,000 to 96,000 Hz, up to eight
channels for enhancement and mono/stereo for separation. Outputs use float WAV/RF64 and exclusive
publication: existing files are preserved. `wait:true` runs a bounded test synchronously; normal
interactive use keeps `wait:false` and polls the job.

## Optional spatial upscale

```json
{"id":"media.upscale","params":{"item":42,"path":"/approved/output/upscaled.mov",
 "width":7680,"height":4320,"fit":"contain","format":"prores"}}
```

This uses FilmCraft's native spatial resampler; it does not reconstruct missing detail with a
neural model. Pixel aspect, Interpret Footage color settings and subclip bounds are honored.
`contain` fits the full image; `cover` fills the output. The output supports up to 8192 pixels per
side and 32 Mi pixels, including 7680×4320. Rate and sample positions remain rational/exact.

A frozen source snapshot supplies the export, PNG preview and JSON provenance. The primary file
keeps an exclusive open handle, so a later path replacement cannot make its worker truncate an
unrelated file. The selected encoder must be available. ProRes and capable HEVC preserve HDR;
formats that would tone-map HDR to SDR require explicit `allowSdr:true`, which is recorded with
the actual output color space. Image sequences and MXF OP-Atom are not single-file upscale targets.

## Editable motion templates

`motion.templates` returns original built-in lower thirds sized to the active sequence. Pass one
to `motion.plan` with text/color controls and an exact timeline tick:

```json
{"id":"motion.plan","params":{"template":{"name":"Title","durationFrames":120,
 "controls":{"title":{"kind":"text","default":"Your title"}},
 "layers":[{"kind":"text","props":{"text":{"control":"title"},"position":[120,920],
 "size":64,"fill_color":[1,1,1,1]},"keys":[{"atFrame":0,"props":{"opacity":0}},
 {"atFrame":12,"props":{"opacity":100}}]}]},"controls":{"title":"Evening set"},"at":0}}
```

The result is a staged edit of native graphics and keyframes, with stable created clip references.
Review its exact frames and approve it as one undo step. Custom declarative templates are bounded
and validate layer kinds, controls, values and keyframes. They cannot execute scripts, load remote
assets or run network calls. All layers remain editable after approval.
