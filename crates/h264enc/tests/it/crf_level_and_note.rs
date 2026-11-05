//! Constant quality has no bitrate cap, so the level must allow what low factors spend; and every
//! default stream says which settings made it.

use crate::common;

use common::synth;
use filmcraft_h264enc::*;

fn crf(w: u32, h: u32, fps: u32, f: f32) -> Encoder {
    let mut c = EncoderConfig::new(w, h, fps, 1);
    c.rate = RateControl::Crf(f);
    c.format = PacketFormat::LengthPrefixed;
    Encoder::new(c).unwrap()
}

#[test]
fn crf_level_allows_the_bitrate_of_the_factor() {
    let level = |w, h, fps, f| crf(w, h, fps, f).avcc()[3];
    // 1080p30: the default and visually lossless factors stay at 4; CRF 0 (measured at 105 Mb/s
    // average, 147 Mb/s peak on camera footage) needs more than 4.2's 62.5 Mb/s
    assert_eq!(level(1920, 1080, 30, 23.0), 40);
    assert_eq!(level(1920, 1080, 30, 18.0), 40);
    assert_eq!(level(1920, 1080, 30, 0.0), 51);
    // never below what the picture size and rate need
    assert_eq!(level(3840, 2160, 30, 51.0), 51);
    // a hostile factor is treated as in range
    assert_eq!(level(1920, 1080, 30, f32::NAN), 40);
    assert_eq!(level(1920, 1080, 30, -100.0), 51);
}

#[test]
fn first_access_unit_carries_the_settings() {
    let (w, h) = (64, 48);
    let mut enc = crf(w as u32, h as u32, 30, 18.0);
    let f = synth(w, h, 0);
    let mut packets = enc.encode(&f.frame(), 0).unwrap();
    packets.extend(enc.flush());
    let text = |p: &Packet| String::from_utf8_lossy(&p.data).into_owned();
    let first = text(&packets[0]);
    for want in ["filmcraft-h264enc", "rc=crf crf=18.0", "profile=high", "level=", "bframes=2", "cabac=1"] {
        assert!(first.contains(want), "{want} missing from {first:?}");
    }
    // once per stream
    assert!(packets.iter().skip(1).all(|p| !text(p).contains("filmcraft-h264enc")));
}

#[test]
fn optional_diagnostic_note_does_not_disable_hdr_metadata() {
    for (diagnostic, hdr, want_sei) in [(true, false, 1), (false, false, 0), (false, true, 1)] {
        let mut cfg = EncoderConfig::new(64, 48, 30, 1);
        cfg.encoder_note = diagnostic;
        cfg.bframes = 0;
        cfg.threads = 1;
        if hdr {
            cfg.mastering_display = Some([0u8; 24]);
            cfg.content_light = Some((1000, 400));
        }
        let mut encoder = Encoder::new(cfg).unwrap();
        let frame = synth(64, 48, 0);
        let mut packets = encoder.encode(&frame.frame(), 0).unwrap();
        packets.extend(encoder.flush());
        let data = &packets[0].data;
        assert_eq!(String::from_utf8_lossy(data).contains("filmcraft-h264enc"), diagnostic);
        let sei: Vec<_> = common::annexb_nals(data).into_iter().filter(|r| data.get(r.start + 3).is_some_and(|v| v & 31 == 6)).collect();
        assert_eq!(sei.len(), want_sei);
        if hdr {
            assert!(sei.iter().any(|r| data.get(r.start + 4) == Some(&137)), "SMPTE ST2086 metadata remains present without diagnostic text");
        }
    }
}
