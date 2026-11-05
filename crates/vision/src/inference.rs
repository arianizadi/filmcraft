use crate::{IMAGE_SIZE, MAX_TEXT_BYTES, Result, models, normalize, preprocess};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::clip::{ClipConfig, ClipModel};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadSpec {
    pub weights: PathBuf,
    pub tokenizer: PathBuf,
    pub format: String,
    pub weights_size: u64,
    pub weights_sha256: String,
}
impl LoadSpec {
    pub fn pinned(dir: &Path) -> Self {
        Self {
            weights: dir.join(models::WEIGHTS.name),
            tokenizer: dir.join(models::TOKENIZER.name),
            format: "pytorch".into(),
            weights_size: models::WEIGHTS.size,
            weights_sha256: models::WEIGHTS.sha256.into(),
        }
    }
    pub fn provenance(&self) -> String {
        format!(
            "OpenAI CLIP ViT-B/32; revision {}; weights SHA256 {}; tokenizer SHA256 {}; local CPU Candle0.9.2/tokenizers0.22.2; preprocessing version1; image preprocessing shortest224-center224-bicubic-sRGB",
            models::REVISION,
            self.weights_sha256,
            models::TOKENIZER.sha256
        )
    }
    pub fn identity(&self) -> String {
        format!("openai/{}@{}:{}:preprocess-v1", models::ID, models::REVISION, self.weights_sha256)
    }
}
pub use LoadSpec as ClipLoadSpec;

pub struct ClipEncoder {
    model: ClipModel,
    tokenizer: Tokenizer,
    spec: LoadSpec,
}
fn error(e: impl std::fmt::Display) -> String {
    format!("CLIP: {e}")
}
fn guarded<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err("CLIP inference/loader failed internally; model state was not changed".into()))
}
impl ClipEncoder {
    pub fn load(spec: LoadSpec, progress: &mut dyn FnMut(&str) -> bool) -> Result<Self> {
        guarded(|| {
            let check = |stage: &str, progress: &mut dyn FnMut(&str) -> bool| if progress(stage) { Ok(()) } else { Err("CLIP loading cancelled".to_string()) };
            if !matches!(spec.format.as_str(), "pytorch" | "safetensors") {
                return Err("format must be pytorch or safetensors".into());
            }
            if spec.format == "pytorch" && (spec.weights_sha256 != models::WEIGHTS.sha256 || spec.weights_size != models::WEIGHTS.size) {
                return Err("PyTorch checkpoints must be the exact pinned OpenAI artifact; custom weights require safe-tensor format and an explicit SHA-256/byte count".into());
            }
            check("Verifying CLIP tokenizer", progress)?;
            let tokenizer_bytes = models::read_verified(&spec.tokenizer, models::TOKENIZER.size, models::TOKENIZER.sha256)?;
            let mut tokenizer = Tokenizer::from_bytes(tokenizer_bytes).map_err(error)?;
            if tokenizer.token_to_id("<|startoftext|>") != Some(49406) || tokenizer.token_to_id("<|endoftext|>") != Some(49407) {
                return Err("CLIP tokenizer has unexpected special-token identities".into());
            }
            tokenizer.with_truncation(Some(TruncationParams { max_length: 77, ..Default::default() })).map_err(error)?;
            tokenizer.with_padding(Some(PaddingParams {
                strategy: PaddingStrategy::Fixed(77),
                pad_id: 49407,
                pad_token: "<|endoftext|>".into(),
                ..Default::default()
            }));
            check("Verifying CLIP model weights", progress)?;
            let device = Device::Cpu;
            let weights = models::read_verified(&spec.weights, spec.weights_size, &spec.weights_sha256)?;
            check("Loading verified CLIP tensors", progress)?;
            let model = if spec.format == "safetensors" {
                let vars = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device).map_err(error)?;
                ClipModel::new(vars, &ClipConfig::vit_base_patch32()).map_err(error)?
            } else {
                drop(weights);
                let vars = VarBuilder::from_pth(&spec.weights, DType::F32, &device).map_err(error)?;
                let model = ClipModel::new(vars, &ClipConfig::vit_base_patch32()).map_err(error)?;
                // Candle's pure-Rust PyTorch tensor reader reopens the path per tensor. Reject
                // any detectable replacement during load before exposing the resulting model.
                models::read_verified(&spec.weights, spec.weights_size, &spec.weights_sha256)?;
                model
            };
            check("CLIP ready", progress)?;
            Ok(Self { model, tokenizer, spec })
        })
    }
    pub fn spec(&self) -> &LoadSpec {
        &self.spec
    }
    pub fn embed_image(&self, width: usize, height: usize, rgba: &[f32]) -> Result<Vec<f32>> {
        guarded(|| {
            let data = preprocess(width, height, rgba)?;
            let input = Tensor::from_vec(data, (1, 3, IMAGE_SIZE, IMAGE_SIZE), &Device::Cpu).map_err(error)?;
            let output = self.model.get_image_features(&input).and_then(|t| t.squeeze(0)).and_then(|t| t.to_vec1::<f32>()).map_err(error)?;
            normalize(output)
        })
    }
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        guarded(|| {
            if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
                return Err(format!("CLIP query must contain 1..{MAX_TEXT_BYTES} bytes"));
            }
            let encoding = self.tokenizer.encode(text, true).map_err(error)?;
            let ids = encoding.get_ids();
            if ids.len() != 77 || ids.iter().any(|v| *v >= 49408) {
                return Err("CLIP tokenizer output exceeds the fixed model vocabulary/context".into());
            }
            let input = Tensor::new(ids, &Device::Cpu).and_then(|t| t.unsqueeze(0)).map_err(error)?;
            let output = self.model.get_text_features(&input).and_then(|t| t.squeeze(0)).and_then(|t| t.to_vec1::<f32>()).map_err(error)?;
            normalize(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_artifacts_fail_before_model_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let spec = LoadSpec::pinned(dir.path());
        assert!(ClipEncoder::load(spec, &mut |_| true).is_err());
        let mut spec = LoadSpec::pinned(dir.path());
        spec.format = "execute-python".into();
        assert!(ClipEncoder::load(spec, &mut |_| true).is_err());
    }
    #[test]
    fn model_is_shareable_and_sendable() {
        fn assert_sync_send<T: Sync + Send>() {}
        assert_sync_send::<ClipEncoder>();
    }
    /// Explicit model opt-in: never downloads during tests. Check a real package once installed.
    #[test]
    #[ignore = "requires opt-in pinned CLIP model; set FILMCRAFT_CLIP_MODEL_DIR"]
    fn real_clip_embeddings_are_finite_unit_vectors_and_retrieve_colors() {
        let dir = std::env::var_os("FILMCRAFT_CLIP_MODEL_DIR").map(PathBuf::from).expect("set FILMCRAFT_CLIP_MODEL_DIR");
        let encoder = ClipEncoder::load(LoadSpec::pinned(&dir), &mut |_| true).unwrap();
        let red = encoder.embed_image(224, 224, &[1.0, 0.0, 0.0, 1.0].repeat(224 * 224)).unwrap();
        let blue = encoder.embed_image(224, 224, &[0.0, 0.0, 1.0, 1.0].repeat(224 * 224)).unwrap();
        let red_text = encoder.embed_text("a completely red image").unwrap();
        let blue_text = encoder.embed_text("a completely blue image").unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>();
        for vector in [&red, &blue, &red_text, &blue_text] {
            assert_eq!(vector.len(), crate::DIMENSION);
            assert!((dot(vector, vector) - 1.0).abs() < 1e-5);
        }
        assert!(dot(&red, &red_text) > dot(&red, &blue_text));
        assert!(dot(&blue, &blue_text) > dot(&blue, &red_text));
        let long = encoder.embed_text(&"A red image. ".repeat(200)).unwrap();
        assert_eq!(long.len(), crate::DIMENSION);
    }
}
