# filmcraft-vision

Local image/text embeddings for FilmCraft's media-library retrieval. This crate does not connect
to a service or download weights while embedding. Native `clip` builds run OpenAI CLIP ViT-B/32
with Candle 0.9.2 on the CPU; featureless builds retain the catalogue, verification and preprocessing.

The model architecture is fixed to ViT-B/32 (512-dimensional projected features). The first-party
package is `openai/clip-vit-base-patch32`, revision
`3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268`. Every artifact has a pinned byte count and SHA-256 in
`models.rs`. The package includes the first-party model card and the original MIT license text.
No weights are bundled with FilmCraft or committed here. The engine exposes an explicit opt-in
download and durable background load; it installs a provider only after verification and successful
model construction. A custom safetensors conversion needs its SHA-256 and byte count and uses the
same pinned tokenizer and fixed architecture. PyTorch parsing is Candle's Rust data parser and does
not execute Python checkpoint code. No native BLAS/CUDA/Python runtime is required. Tokenizers uses
its pure Rust fancy-regex path; Oniguruma and esaxx C++ features are disabled.

Preprocessing follows the pinned OpenAI/Hugging Face CLIP processor: encode the displayed linear
frame to 8-bit sRGB over black, bicubic (Catmull–Rom) resize of shortest side to 224, center crop to
224, channel-first float values with the model's published mean and standard deviation. The resize
uses the Rust image library; its rounding can differ slightly from Pillow. Text uses the exact
verified CLIP BPE tokenizer, 77-token context with start/end tokens, and right truncation. Image and
text outputs are checked for finite values and normalized to unit L2 length.

Semantic scores describe sampled-frame similarity, not calibrated probabilities or guaranteed
object/action labels. The official model card is English-focused and describes limitations in
counting, fine detail and task-dependent precision. Sparse frame sampling can miss short actions;
source time ranges and thumbnails remain visible for review. Transcript evidence is indexed
separately by the engine and remains usable without a vision model.

Primary specifications and API references:

- [OpenAI CLIP paper, ICML 2021](https://arxiv.org/abs/2103.00020)
- [OpenAI model card](https://github.com/openai/CLIP/blob/main/model-card.md)
- [OpenAI MIT license](https://github.com/openai/CLIP/blob/b1c4b6be5871f1b94359ba55901627f29ecc9ae9/LICENSE)
- [Pinned processor configuration](https://huggingface.co/openai/clip-vit-base-patch32/blob/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/preprocessor_config.json)
- [Candle CLIP API](https://docs.rs/candle-transformers/0.9.2/candle_transformers/models/clip/index.html)

`cargo test -p filmcraft-vision --features clip` runs bounded-preprocessing, corrupt-artifact,
checksum and feature checks without downloads. The ignored real-model test checks actual local
inference, normalization, red/blue image retrieval, and long-query truncation:

```sh
FILMCRAFT_CLIP_MODEL_DIR='/absolute/path/to/clip-vit-base-patch32' \
  cargo test -p filmcraft-vision --features clip real_clip_embeddings -- --ignored --nocapture
```
