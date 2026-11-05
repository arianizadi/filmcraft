//! Raw syncframe imports use our existing AC-3/E-AC-3 decoder. External ffmpeg only generates
//! small synthetic tones and acts as an independent decode oracle; no binary fixture is shipped.
use std::sync::Arc;

use filmcraft_media::MediaKind;

use super::common;

fn generated(codec: &str, rate: &str, channels: &str) -> std::path::PathBuf {
    let ff = filmcraft_testkit::ffmpeg().expect("external ffmpeg for fixture");
    common::fixture(
        &ff,
        &format!("raw_{codec}_{rate}_{channels}.{}", if codec == "eac3" { "eac3" } else { "ac3" }),
        &["-f", "lavfi", "-i", &format!("sine=frequency=440:sample_rate={rate}"), "-t", "0.24", "-ac", channels, "-c:a", codec, "-b:a", "192k", "-f", codec],
    )
    .expect("generate raw Dolby fixture")
}

#[test]
fn standalone_ac3_eac3_metadata_and_decoding_match_oracle() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    for (codec, rate, channels) in [("ac3", 48_000u32, 2u32), ("eac3", 32_000, 1)] {
        let path = generated(codec, &rate.to_string(), &channels.to_string());
        let bytes = common::bytes(&path);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(filmcraft_media::is_importable(&path));
        let source = filmcraft_codecs::open_bytes(name, bytes.clone()).unwrap();
        assert_eq!(source.info().kind, MediaKind::AudioOnly);
        let info = source.info().audio().unwrap();
        assert_eq!((info.sample_rate, info.channels), (rate, channels));
        assert_eq!(info.codec, if codec == "eac3" { "E-AC-3" } else { "AC-3" });
        let want = common::ffmpeg_audio_f32(&ff, &path, &[]);
        let frames = want.len() / channels as usize;
        assert_eq!(source.info().duration.to_units_floor(i64::from(rate)), frames as i64);
        let ours = source.audio(0, frames, rate).unwrap();
        let (mut signal, mut error) = (0f64, 0f64);
        for (index, &value) in want.iter().enumerate().skip(1024) {
            let channel = index % channels as usize;
            let sample = index / channels as usize;
            let difference = f64::from(ours.channels[channel][sample]) - f64::from(value);
            signal += f64::from(value).powi(2);
            error += difference * difference;
        }
        let snr = 10.0 * (signal / error.max(1e-30)).log10();
        assert!(snr > 30.0, "{codec}: SNR {snr:.2} dB");
        let reader: filmcraft_media::SharedReader = Arc::new(filmcraft_media::reader::MemReader(bytes));
        let streamed = filmcraft_media::reader::open_reader(name, reader, &filmcraft_codecs::reader_openers(), &filmcraft_codecs::openers()).unwrap();
        let window = streamed.audio(500, 1000, rate).unwrap();
        assert_eq!(window.channels[0], ours.channels[0][500..1500]);
        for (start, frames, output_rate) in [(0, usize::MAX, rate), (i64::MAX, 10, rate), (0, 10, 0), (0, 10, u32::MAX)] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.audio(start, frames, output_rate)));
            assert!(result.is_ok(), "hostile audio read panicked");
            assert!(result.unwrap().is_err());
        }
    }
}

#[test]
fn standalone_raw_dolby_damage_is_rejected_without_panicking() {
    let _ff = filmcraft_testkit::require_ffmpeg!();
    for codec in ["ac3", "eac3"] {
        let good = std::fs::read(generated(codec, "48000", "2")).unwrap();
        let extension = if codec == "eac3" { "EC3" } else { "AC3" };
        let name = format!("damaged.{extension}");
        let mut cases: Vec<Vec<u8>> = (0..good.len()).step_by(113).map(|n| good[..n].to_vec()).collect();
        for index in (0..good.len()).step_by(127) {
            let mut damaged = good.clone();
            damaged[index] ^= 1 << (index % 8);
            cases.push(damaged);
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(filmcraft_codecs::open_bytes(&name, Arc::from(trailing)).is_err());
        if codec == "eac3" {
            for flag in [0x40, 0x08] {
                let mut dependent = good.clone();
                dependent[2] |= flag;
                let error = filmcraft_codecs::open_bytes(&name, Arc::from(dependent)).err().unwrap().to_string();
                assert!(error.contains("substream"), "{error}");
            }
        }
        for (index, bytes) in cases.into_iter().enumerate() {
            let result = std::panic::catch_unwind(|| filmcraft_codecs::open_bytes(&name, Arc::from(bytes)).map(|s| s.audio(0, 400, 48000).is_ok()));
            assert!(result.is_ok(), "{codec} mutation {index} panicked");
        }
    }
}
