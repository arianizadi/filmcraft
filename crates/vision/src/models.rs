//! First-party OpenAI CLIP artifacts, pinned to immutable revisions and SHA-256.
use crate::Result;
use serde::Serialize;
use sha2::Digest;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const ID: &str = "clip-vit-base-patch32";
pub const REVISION: &str = "3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268";
pub const LICENSE: &str = "MIT";
pub const LICENSE_URL: &str = "https://github.com/openai/CLIP/blob/main/LICENSE";
pub const MODEL_CARD: &str = "https://huggingface.co/openai/clip-vit-base-patch32/blob/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/README.md";
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Artifact {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}
pub const WEIGHTS: Artifact = Artifact {
    name: "pytorch_model.bin",
    url: "https://huggingface.co/openai/clip-vit-base-patch32/resolve/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/pytorch_model.bin",
    sha256: "a63082132ba4f97a80bea76823f544493bffa8082296d62d71581a4feff1576f",
    size: 605_247_071,
};
pub const TOKENIZER: Artifact = Artifact {
    name: "tokenizer.json",
    url: "https://huggingface.co/openai/clip-vit-base-patch32/resolve/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/tokenizer.json",
    sha256: "b556ac8c99757ffb677208af34bc8c6721572114111a6e0aaf5fa69ff0b8d842",
    size: 2_224_041,
};
pub const MODEL_CARD_FILE: Artifact = Artifact {
    name: "MODEL-CARD.md",
    url: "https://huggingface.co/openai/clip-vit-base-patch32/resolve/3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268/README.md",
    sha256: "8f0e8c678e0afe5f384f3bf8a6694c1cb53c41e52f86277967b6c5354cb0f9f4",
    size: 7_942,
};
pub const LICENSE_FILE: Artifact = Artifact {
    name: "LICENSE.txt",
    url: "https://raw.githubusercontent.com/openai/CLIP/b1c4b6be5871f1b94359ba55901627f29ecc9ae9/LICENSE",
    sha256: "987e63b32f6c89ff5160e429458a872ff048e6860b590a3912e938f9da8f14db",
    size: 1_064,
};
pub const FILES: &[Artifact] = &[WEIGHTS, TOKENIZER, MODEL_CARD_FILE, LICENSE_FILE];
pub const MAX_MODEL_BYTES: u64 = 800 * 1024 * 1024;
pub fn dir(models_dir: &Path) -> PathBuf {
    models_dir.join(ID)
}
pub fn catalogue() -> serde_json::Value {
    serde_json::json!({"id":ID,"name":"OpenAI CLIP ViT-B/32","revision":REVISION,"dimension":crate::DIMENSION,"license":LICENSE,"licenseUrl":LICENSE_URL,"modelCard":MODEL_CARD,"files":FILES,"bytes":FILES.iter().map(|a|a.size).sum::<u64>(),"inference":"local CPU, pure Rust Candle","language":"English","limitations":"Ranked visual similarity; sampled frames can miss actions and details. Scores are not calibrated probabilities. Review results before editing.","preprocessing":{"resizeShortest":224,"centerCrop":224,"resize":"bicubic","pixelEncoding":"sRGB","mean":[0.48145466,0.4578275,0.40821073],"std":[0.26862954,0.26130258,0.27577711]}})
}
/// Verify the same open file used for the bounded read. Callers never allocate from an
/// unvalidated file header or trust a digest supplied by a model file itself.
pub fn read_verified(path: &Path, expected_size: u64, sha: &str) -> Result<Vec<u8>> {
    if expected_size == 0 || expected_size > MAX_MODEL_BYTES || sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid model byte bound or SHA-256".into());
    }
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() != expected_size {
        return Err(format!("{}: expected {expected_size} bytes", path.display()));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(expected_size as usize).map_err(|e| format!("model allocation: {e}"))?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.len().checked_add(count).is_none_or(|n| n as u64 > expected_size) {
            return Err("model grew beyond its declared byte bound while reading".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as u64 != expected_size || !format!("{:x}", sha2::Sha256::digest(&bytes)).eq_ignore_ascii_case(sha) {
        return Err(format!("{}: SHA-256/size mismatch; file was not loaded", path.display()));
    }
    Ok(bytes)
}
pub fn verify_file(path: &Path, artifact: Artifact) -> Result<()> {
    let bytes = read_verified(path, artifact.size, artifact.sha256)?;
    drop(bytes);
    Ok(())
}
pub fn present(models_dir: &Path) -> bool {
    let dir = dir(models_dir);
    FILES.iter().all(|file| dir.join(file.name).metadata().is_ok_and(|m| m.len() == file.size && m.is_file()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_hash_size_and_mutated_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        std::fs::write(&path, b"abc").unwrap();
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(read_verified(&path, 3, sha).unwrap(), b"abc");
        for (size, hash) in [(4, sha), (MAX_MODEL_BYTES + 1, sha), (3, "not a hash")] {
            assert!(read_verified(&path, size, hash).is_err());
        }
        std::fs::write(&path, b"abd").unwrap();
        assert!(read_verified(&path, 3, sha).is_err());
    }
    #[test]
    fn catalogue_is_immutable_and_pinned() {
        for a in FILES {
            assert!(a.url.contains(REVISION) || a.name == "LICENSE.txt");
            assert_eq!(a.sha256.len(), 64);
            assert!(a.size <= MAX_MODEL_BYTES);
        }
        assert_eq!(REVISION.len(), 40);
    }
}
