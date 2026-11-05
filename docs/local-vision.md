# Local semantic media search

FilmCraft can produce CLIP embeddings on this computer with the native `vision` feature. Image and
text inputs stay local. Models are never downloaded automatically. `library.model.status` returns
the pinned catalogue, package size, model card, license, local files and installed provider identity.

After inspecting that information and choosing the download, run:

```json
{"id":"library.model.download","params":{"acceptDownload":true}}
```

This returns a stable job ID. Inspect it through `jobs.get` or `jobs.list`, and use `jobs.cancel` to
cancel. The first-party package is about 608 MB; each file is checked against an immutable revision,
byte count and SHA-256 before installation. The package includes the MIT license, model card, per-file attribution sidecars and a local
`ATTRIBUTION.md` index.
An interrupted job is visible after restart and does not resume automatically. Files live in the
application's model directory; `directory` can choose an explicit model package folder.

Once the download completes, load it explicitly:

```json
{"id":"library.model.load","params":{}}
```

Loading is also a durable background job. Its status records the input project revision and provider
identity. A failed load retains any previously loaded provider. `library.model.unload` frees the
provider and cancels a pending load while retaining downloaded files. Opening another project
keeps the session's model ready and clears document-specific indexed ranges.

Index approved source items with real image/text embeddings in a background job:

```json
{"id":"library.indexAsync","params":{"items":[12,18],"maxFrames":24,"embeddings":true}}
{"id":"library.search","params":{"query":"a wide shot of the crowd","limit":8,"thumbnails":true}}
```

Wait for the returned durable job to finish; `library.indexStatus` reports cache installation and
any stale-result refusal. The worker uses a fixed project snapshot and installs its bounded result
only when project identity, revision, index generation, provider and source fingerprints still match.
Cancellation is checked between frames and propagated to decoders. `library.index` keeps the
bounded synchronous API for small measured jobs and scripts.

The index is incremental and fingerprints source media and transcripts. Search returns source
ranges, thumbnails, transcript evidence and ranked similarity. Use the returned ranges in
`library.selectsPlan` or `library.brollPlan` for timeline review. Only compatible vectors from the
same model identity are compared. The cosine similarity is not a calibrated confidence score.
The model is optimized for English descriptions and can miss small details, counts and short
motions between sampled frames. Review the evidence before applying a cut.

Without a loaded model, transcript and measured-descriptor retrieval remain available. For another
machine or a converted package, `library.model.load` can accept explicit safetensors `weights` and
`tokenizer` paths, `format:"safetensors"`, `weightsBytes` and `weightsSha256`; it still requires the
verified first-party tokenizer and ViT-B/32 architecture. No arbitrary model code or Python runs.
