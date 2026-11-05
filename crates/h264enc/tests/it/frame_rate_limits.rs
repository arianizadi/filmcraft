//! High-frame-rate delivery must fail rather than emit an incorrect level label or timing.
use filmcraft_h264enc::{Encoder, EncoderConfig, RateControl};

fn config(width: u32, height: u32, num: u32, den: u32) -> EncoderConfig {
    let mut cfg = EncoderConfig::new(width, height, num, den);
    cfg.threads = 1;
    cfg.rate = RateControl::Qp(23);
    cfg
}

#[test]
fn supported_1080p960_and_8k60_keep_valid_automatic_level_selection() {
    for (width, height, fps) in [(1920, 1080, 960), (1920, 1080, 2000), (7680, 4320, 60)] {
        let encoder = Encoder::new(config(width, height, fps, 1)).unwrap();
        assert_eq!(encoder.avcc().get(3), Some(&62));
    }
}

#[test]
fn exceeding_all_supported_levels_is_an_actionable_error() {
    for (width, height, fps) in [(3840, 2160, 960), (7680, 4320, 240), (16384, 16384, 24)] {
        for requested in [None, Some(62)] {
            let mut cfg = config(width, height, fps, 1);
            cfg.level = requested;
            let error = Encoder::new(cfg).err().expect("unsupported level requirements must fail");
            assert!(error.to_string().contains("exceed every implemented H.264 level"), "{error}");
        }
    }
    let mut cfg = config(1920, 1080, 30, 1);
    cfg.rate = RateControl::Cbr { kbps: 1_000_001 };
    assert!(Encoder::new(cfg).is_err(), "High 6.2 cannot signal a bitrate above 1 Gbit/s");
}
