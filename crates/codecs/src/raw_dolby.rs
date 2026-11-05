//! Standalone AC-3 / E-AC-3 syncframe streams. Framing and decoding use our existing
//! ATSC A/52:2012 decoder API; no third-party codec implementation is read or linked.
//! This importer accepts one complete primary program with constant sample rate/layout.
//! Damage, truncation, extra programs and dependent substreams fail explicitly, rather than
//! shortening audio or silently dropping channels.

use std::ops::Range;
use std::sync::Arc;

use filmcraft_ac3::Header;
use filmcraft_media::{MediaError, SharedReader, SharedSource};

use crate::{CodecError, Result};

pub(crate) const MAX_RAW_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SAMPLES: usize = 64 * 1024 * 1024; // 256 MiB decoded, across all channels.

pub(crate) fn extension(name: &str) -> bool {
    name.rsplit('.').next().is_some_and(|ext| ["ac3", "eac3", "ec3"].iter().any(|v| ext.eq_ignore_ascii_case(v)))
}

fn failure(message: impl Into<String>) -> CodecError {
    CodecError::Decode(format!("raw AC-3/E-AC-3: {}", message.into()))
}

fn budget(samples: usize, channels: usize) -> Result<()> {
    if samples.checked_mul(channels).is_none_or(|n| n > MAX_SAMPLES) {
        return Err(CodecError::Unsupported("raw AC-3/E-AC-3 exceeds 256 MiB decoded; remux to MP4, MKV or MPEG-TS for longer audio".into()));
    }
    Ok(())
}

fn index(bytes: &[u8]) -> Result<(Header, Vec<Range<usize>>, usize)> {
    if bytes.len() as u64 > MAX_RAW_BYTES {
        return Err(CodecError::Unsupported("raw AC-3/E-AC-3 exceeds 128 MiB encoded; remux to MP4, MKV or MPEG-TS".into()));
    }
    let (mut offset, mut total) = (0usize, 0usize);
    let (mut first, mut frames) = (None::<Header>, Vec::new());
    while offset < bytes.len() {
        let rest = bytes.get(offset..).ok_or_else(|| failure("invalid frame offset"))?;
        let header = filmcraft_ac3::parse_header(rest).map_err(|e| failure(format!("at byte {offset}: {e}")))?;
        if !header.is_primary() {
            return Err(CodecError::Unsupported("standalone E-AC-3 accepts only primary independent substream 0; dependent substreams/additional programs are unsupported; extract a single primary stream first".into()));
        }
        if let Some(first) = first {
            if first.sample_rate != header.sample_rate
                || first.channels() != header.channels()
                || first.acmod != header.acmod
                || first.is_eac3() != header.is_eac3()
            {
                return Err(CodecError::Unsupported("raw AC-3/E-AC-3 changes codec, sample rate or channel layout; separate the streams or remux them".into()));
            }
        } else {
            first = Some(header);
        }
        if header.frame_bytes < 7 || header.samples() == 0 || header.sample_rate == 0 {
            return Err(failure("invalid frame dimensions"));
        }
        let end =
            offset.checked_add(header.frame_bytes).filter(|v| *v <= bytes.len()).ok_or_else(|| failure(format!("truncated syncframe at byte {offset}")))?;
        total = total.checked_add(header.samples()).ok_or_else(|| failure("sample count overflow"))?;
        budget(total, header.channels())?;
        frames.try_reserve(1).map_err(|e| failure(format!("cannot allocate frame index: {e}")))?;
        frames.push(offset..end);
        offset = end;
    }
    let first = first.ok_or_else(|| failure("no syncframes"))?;
    Ok((first, frames, total))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<(Header, Vec<Vec<f32>>)> {
    let (header, frames, samples) = index(bytes)?;
    let mut output = vec![Vec::new(); header.channels()];
    for channel in &mut output {
        channel.try_reserve_exact(samples).map_err(|e| failure(format!("cannot allocate decoded samples: {e}")))?;
    }
    let mut decoder = filmcraft_ac3::Decoder::new();
    for range in frames {
        let frame = bytes.get(range.clone()).ok_or_else(|| failure("invalid indexed frame"))?;
        let decoded = decoder.decode(frame).map_err(|e| failure(format!("at byte {}: {e}", range.start)))?;
        if decoded.channels.len() != output.len() || decoded.channels.iter().any(|c| c.len() != decoded.header.samples() || c.iter().any(|s| !s.is_finite())) {
            return Err(failure("decoder returned an inconsistent sample layout"));
        }
        for (out, channel) in output.iter_mut().zip(decoded.channels) {
            out.extend(channel);
        }
    }
    Ok((header, output))
}

/// Refuse oversized raw files before the generic reader fallback reads them in full.
pub(crate) fn reader_opener(name: &str, reader: &SharedReader) -> Option<std::result::Result<SharedSource, MediaError>> {
    if !extension(name) {
        return None;
    }
    Some((|| {
        if reader.len() > MAX_RAW_BYTES {
            return Err(MediaError::Unsupported("raw AC-3/E-AC-3 exceeds 128 MiB encoded; remux to MP4, MKV or MPEG-TS".into()));
        }
        let len = usize::try_from(reader.len()).map_err(|_| MediaError::Unsupported("raw AC-3/E-AC-3 length is not representable".into()))?;
        let bytes = filmcraft_media::reader::read_range(&**reader, 0, len).map_err(|e| MediaError::Io(e.to_string()))?;
        super::audio::AudioFileSource::decode(name, Arc::from(bytes)).map(|s| Arc::new(s) as SharedSource).map_err(Into::into)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Oversized(AtomicUsize);
    impl filmcraft_media::reader::ByteReader for Oversized {
        fn len(&self) -> u64 {
            MAX_RAW_BYTES + 1
        }
        fn read_at(&self, _: u64, _: &mut [u8]) -> std::io::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(std::io::Error::other("must never read oversized raw audio"))
        }
    }

    #[test]
    fn hostile_raw_sizes_fail_before_read_or_allocation() {
        assert!(budget(usize::MAX, 6).is_err());
        assert!(budget(MAX_SAMPLES + 1, 1).is_err());
        assert!(budget(MAX_SAMPLES / 6, 6).is_ok());
        let original = Arc::new(Oversized(AtomicUsize::new(0)));
        let reader: SharedReader = original.clone();
        assert!(reader_opener("huge.EAC3", &reader).unwrap().is_err());
        assert_eq!(original.0.load(Ordering::Relaxed), 0);
    }
}
