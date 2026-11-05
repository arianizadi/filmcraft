# Media compatibility and runtime verification

FilmCraft has separate container readers, video/audio decoders, and exporters. Recognizing a file
extension or reading its duration does not establish that its streams decode. The installed
platform can add hardware support; the software path and runtime availability remain distinct.

Use the following reproducible audit with the exact CLI binary you intend to use:

```sh
python3 scripts/test-format-compatibility.py \
  --binary target/release/filmcraft-cli \
  --output-dir "$HOME/Desktop/FilmCraft Tests" \
  --ffmpeg /opt/homebrew/bin/ffmpeg --ffprobe /opt/homebrew/bin/ffprobe
```

Both Python scripts in `scripts/` are required. Python uses only its standard library. FFmpeg is an
external fixture generator and independent oracle: it is never linked into, bundled with, or
invoked by FilmCraft's import/export implementation. Generated media and logs stay in a unique
output directory and are not repository assets.

The audit generates small test patterns and tones, imports them through FilmCraft MCP, renders
actual frames with no offline slate, and compares the displayed pattern with an independent
decode. Audio must produce a non-silent measured peak **and** an audible PCM export that the
oracle decodes. Export checks write files and make the oracle decode their streams, including
separate OP-Atom audio files. Results distinguish `verified`, `partial`, `unsupported_or_failed`,
and `fixture_unavailable`. The last status means the external generator lacks an encoder; it
says nothing about FilmCraft's decoder. Consult the saved per-stream profiles and errors.

## Implemented import paths

These are source-backed capability families, not a guarantee about every possible profile.
`report.json` supplies the verified boundary for the current binary and generated streams.

| Container/source family | Implemented paths | Material boundaries |
|---|---|---|
| MP4/MOV | H.264, HEVC, AV1, ProRes, DNx, APV, MJPEG/raw; supported audio sample entries | Codec profile still matters; encrypted/protected media is not supported |
| MKV/WebM | H.264, HEVC, VP8, VP9, AV1, ProRes, MJPEG; AAC, Opus, Vorbis, FLAC, MPEG audio, PCM, AC-3/E-AC-3 | FFV1 and DTS are not implemented |
| AVI/OpenDML | MJPEG, H.264, HEVC, uncompressed RGB/YUV; PCM, MP3/MP2, AC-3 | DivX/Xvid and DV are not implemented |
| MXF | OP1a/OP-Atom, AVC, MPEG-2, DNx, ProRes, PCM/AES3 and timecode | This does not cover every camera-specific MXF essence or metadata dialect |
| MPEG TS/PS | H.264, HEVC, MPEG-1/2; MPEG audio, AAC ADTS/LATM, AC-3/E-AC-3, LPCM | No DTS; spanned camera-folder workflows remain separate |
| Ogg | Opus and Vorbis, granule-position seeking | Ogg is not a general video-container implementation here |
| Raw video | MPEG-1/2 elementary streams, APV raw, YUV4MPEG2 | Exact format identification remains required |
| Audio files | WAV/BWF, AIFF/AIFC, MP3, FLAC, AAC ADTS/M4A, ALAC, Ogg Vorbis/Opus, raw AC-3/E-AC-3 | WMA is not implemented; raw AC-3/E-AC-3 accepts one constant primary program with bounded in-memory decoding |
| Stills/sequences | PNG, JPEG, TIFF, BMP, GIF/APNG, WebP, Targa, DPX, OpenEXR, Radiance HDR, DNG; flattened PSD subset | No HEIC/HEIF; EXR/PSD/TIFF variants have documented limits |
| ASF/WMV, FLV/F4V, DCP | No reader | Do not treat an extension rename as conversion |

Video decoder boundaries include software H.264 8-bit 4:2:0 and HEVC Main/Main10 4:2:0.
HEVC RExt 4:2:2/4:4:4/12-bit, SCC and multilayer are unsupported by the software decoder.
VP9 supports profiles 0–3; AV1 supports Main-profile decoding. ProRes and DNx support broader
editing profiles as documented in their crate READMEs. Platform hardware may accept additional
streams and can fall back to software; its effective behavior must be tested on the target Mac.

E-AC-3 is implemented, rather than universally missing: the clean-room decoder supports the
primary independent substream and spectral extension. Dependent substreams and further programs
are skipped; adaptive hybrid transform and enhanced coupling return unsupported errors. See
[`crates/ac3`](../crates/ac3/README.md) and its integration tests for the precise boundary.

## Implemented output families

| Output | Availability |
|---|---|
| H.264 + AAC, MP4/MOV | Software encoder, optional platform hardware |
| HEVC, MP4/MOV | Registered hardware encoder required; HDR/Main10 requires a separate runtime capability |
| AV1, MP4 | Registered hardware encoder required; unavailable on the tested Mac binary |
| ProRes, DNxHR, APV, MJPEG, MOV | Built-in encoders; profile choices and alpha support differ |
| MXF OP1a/OP-Atom | Built-in DNxHR, ProRes or H.264 picture with PCM; OP-Atom writes separate audio files |
| PNG/TIFF/BMP/JPEG/Targa/DPX/OpenEXR sequences, GIF | Built-in; transparency and EXR compression choices vary |
| WAV, AIFF, AAC M4A/ADTS, FLAC | Built-in audio exports |
| MP3, WMA, MKV/WebM, AVI, MPEG TS/PS | No corresponding exporter |

A local macOS audit exercised H.264, HEVC, ProRes, DNxHR, APV, MJPEG, both MXF modes, all listed
image output families, WAV, AIFF, AAC and FLAC through actual export and independent decoding.
That verifies the generated small fixtures; native 8K/HDR, long-GOP camera footage and sustained
high-frame-rate delivery require their own tests. The separate
[`test-agent-workflow.py`](../scripts/test-agent-workflow.py) preserves an original footage file
while creating a native-size/rate test timeline, reviewed edits, before/after frames, and a short
explicit SDR proxy.

For codec-level accuracy beyond smoke checks, run the existing external-oracle suites:

```sh
FILMCRAFT_FFMPEG=/opt/homebrew/bin/ffmpeg \
FILMCRAFT_FFPROBE=/opt/homebrew/bin/ffprobe \
FILMCRAFT_REQUIRE_ORACLES=1 cargo test -p filmcraft-codecs --test it
FILMCRAFT_REQUIRE_ORACLES=1 cargo test -p filmcraft-ac3 --test oracle
```

The codechub suite checks seeking, channel layouts, presentation timing, decoder output and
sample alignment. Missing optional **fixture encoders** can still skip individual fixture cases;
read the output rather than counting a passed test as universal coverage.
