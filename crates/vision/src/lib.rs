//! Verified local CLIP embeddings. Models are opt-in downloads, never bundled or fetched here.
//! Input frames are display-linear premultiplied RGBA; preprocessing composites over black,
//! encodes sRGB, bicubic-resizes the shortest side to 224, center-crops, and normalizes CHW.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

#[cfg(all(feature = "clip", not(target_arch = "wasm32")))]
mod inference;
pub mod models;
#[cfg(all(feature = "clip", not(target_arch = "wasm32")))]
pub use inference::{ClipEncoder, ClipLoadSpec};

pub type Result<T> = std::result::Result<T, String>;
pub const IMAGE_SIZE: usize = 224;
pub const DIMENSION: usize = 512;
pub const MAX_TEXT_BYTES: usize = 8192;

/// CLIP's specified preprocessing in display RGB, with bounded source/resize allocations.
pub fn preprocess(width: usize, height: usize, rgba: &[f32]) -> Result<Vec<f32>> {
    let pixels = width.checked_mul(height).filter(|n| *n > 0 && *n <= 16_777_216).ok_or("invalid CLIP image dimensions; limit is 16 megapixels")?;
    if width > 8192 || height > 8192 || pixels.checked_mul(4) != Some(rgba.len()) || rgba.iter().any(|v| !v.is_finite()) {
        return Err("CLIP image shape or finite pixel data is invalid".into());
    }
    let shortest = width.min(height);
    let new_width = width.checked_mul(IMAGE_SIZE).ok_or("resize overflow")? / shortest;
    let new_height = height.checked_mul(IMAGE_SIZE).ok_or("resize overflow")? / shortest;
    if new_width.max(new_height) > 16_384 || new_width.checked_mul(new_height).is_none_or(|n| n > 4_194_304) {
        return Err("CLIP image aspect ratio is excessive".into());
    }
    let mut rgb = Vec::with_capacity(pixels * 3);
    for pixel in rgba.as_chunks::<4>().0 {
        if !(0.0..=1.0).contains(&pixel[3]) {
            return Err("CLIP image alpha must be in 0..1".into());
        }
        for value in pixel.iter().take(3) {
            rgb.push((filmcraft_color::linear_to_srgb(value.max(0.0)).clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    let image = image::RgbImage::from_raw(width as u32, height as u32, rgb).ok_or("could not construct CLIP input image")?;
    let resized = image::imageops::resize(&image, new_width as u32, new_height as u32, image::imageops::FilterType::CatmullRom);
    // PIL's center crop rounds half-pixel offsets to the nearest even integer.
    let left = ((new_width - IMAGE_SIZE) as f64 / 2.0).round_ties_even() as usize;
    let top = ((new_height - IMAGE_SIZE) as f64 / 2.0).round_ties_even() as usize;
    let means = [0.48145466, 0.4578275, 0.40821073];
    let stds = [0.26862954, 0.261_302_6, 0.275_777_1];
    let mut out = vec![0.0; 3 * IMAGE_SIZE * IMAGE_SIZE];
    for y in 0..IMAGE_SIZE {
        for x in 0..IMAGE_SIZE {
            let pixel = resized.get_pixel((left + x) as u32, (top + y) as u32);
            for channel in 0..3 {
                out[channel * IMAGE_SIZE * IMAGE_SIZE + y * IMAGE_SIZE + x] = (f32::from(pixel[channel]) / 255.0 - means[channel]) / stds[channel];
            }
        }
    }
    Ok(out)
}

pub fn normalize(mut vector: Vec<f32>) -> Result<Vec<f32>> {
    if vector.len() != DIMENSION || vector.iter().any(|v| !v.is_finite()) {
        return Err("CLIP returned an invalid embedding".into());
    }
    let norm = vector.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
    if !norm.is_finite() || norm < 1e-20 {
        return Err("CLIP embedding norm is invalid".into());
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preprocessing_matches_declared_constant_color_normalization() {
        let image = [0.0, 0.0, 0.0, 1.0].repeat(IMAGE_SIZE * IMAGE_SIZE);
        let pre = preprocess(IMAGE_SIZE, IMAGE_SIZE, &image).unwrap();
        assert_eq!(pre.len(), 3 * IMAGE_SIZE * IMAGE_SIZE);
        for (channel, (mean, std)) in [0.48145466, 0.4578275, 0.40821073].into_iter().zip([0.26862954, 0.261_302_6, 0.275_777_1]).enumerate() {
            assert!(pre[channel * IMAGE_SIZE * IMAGE_SIZE..(channel + 1) * IMAGE_SIZE * IMAGE_SIZE].iter().all(|v| (*v + mean / std).abs() < 1e-6));
        }
    }
    #[test]
    fn hostile_pixels_dimensions_and_vectors_are_errors() {
        for (w, h, px) in [(usize::MAX, 2, vec![]), (0, 1, vec![]), (1, 1, vec![f32::NAN, 0.0, 0.0, 1.0]), (1, 1, vec![0.0, 0.0, 0.0, -1.0])] {
            assert!(std::panic::catch_unwind(|| preprocess(w, h, &px)).unwrap().is_err());
        }
        assert!(normalize(vec![0.0; DIMENSION]).is_err());
        assert!(normalize(vec![f32::INFINITY; DIMENSION]).is_err());
        assert!(normalize(vec![1.0; 4]).is_err());
    }
}
