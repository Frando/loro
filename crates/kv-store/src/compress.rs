use std::io::{self, Read, Write};

use bytes::Bytes;
use loro_common::LoroError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None,
    LZ4,
}

impl CompressionType {
    pub fn is_none(&self) -> bool {
        matches!(self, CompressionType::None)
    }
}

impl TryFrom<u8> for CompressionType {
    type Error = LoroError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CompressionType::None),
            1 => Ok(CompressionType::LZ4),
            _ => Err(LoroError::DecodeError(
                format!("Invalid compression type: {value}").into(),
            )),
        }
    }
}

impl From<CompressionType> for u8 {
    fn from(value: CompressionType) -> Self {
        match value {
            CompressionType::None => 0,
            CompressionType::LZ4 => 1,
        }
    }
}

pub fn compress(w: &mut Vec<u8>, data: &[u8], compression_type: CompressionType) {
    match compression_type {
        CompressionType::None => {
            w.write_all(data).unwrap();
        }
        CompressionType::LZ4 => {
            let mut encoder = lz4_flex::frame::FrameEncoder::new(w);
            encoder.write_all(data).unwrap();
            let _w = encoder.finish().unwrap();
        }
    }
}

pub fn decompress(
    out: &mut Vec<u8>,
    data: Bytes,
    compression_type: CompressionType,
) -> Result<(), LoroError> {
    match compression_type {
        CompressionType::None => {
            out.write_all(&data).unwrap();
            Ok(())
        }
        CompressionType::LZ4 => {
            let mut decoder = lz4_flex::frame::FrameDecoder::new(data.as_ref());
            io::copy(&mut decoder, out)
                .map_err(|e| LoroError::DecodeError(e.to_string().into()))?;
            Ok(())
        }
    }
}

/// Decompresses `data` into `out` like [`decompress`], but stops once the
/// decompressed output would pass `limit` bytes.
///
/// The output is read as a bounded stream, so a small input that expands to
/// a huge output (an LZ4 bomb) allocates at most about `limit` bytes before
/// it is refused. Returns the number of bytes appended to `out`, or `None`
/// (with `out` unchanged) if the output is larger than `limit`.
pub fn decompress_with_limit(
    out: &mut Vec<u8>,
    data: Bytes,
    compression_type: CompressionType,
    limit: usize,
) -> Result<Option<usize>, LoroError> {
    let start = out.len();
    match compression_type {
        CompressionType::None => {
            if data.len() > limit {
                return Ok(None);
            }
            out.extend_from_slice(&data);
        }
        CompressionType::LZ4 => {
            let decoder = lz4_flex::frame::FrameDecoder::new(data.as_ref());
            let mut bounded = decoder.take((limit as u64).saturating_add(1));
            if let Err(e) = io::copy(&mut bounded, out) {
                out.truncate(start);
                return Err(LoroError::DecodeError(e.to_string().into()));
            }
            if out.len() - start > limit {
                out.truncate(start);
                return Ok(None);
            }
        }
    }
    Ok(Some(out.len() - start))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn decompression_stops_at_the_limit() {
        let data = vec![0u8; 16 << 20];
        let mut compressed = Vec::new();
        compress(&mut compressed, &data, CompressionType::LZ4);
        assert!(compressed.len() < 1 << 20);
        let compressed = Bytes::from(compressed);

        let mut out = vec![7];
        let got =
            decompress_with_limit(&mut out, compressed.clone(), CompressionType::LZ4, 1 << 20);
        assert_eq!(got.unwrap(), None);
        assert_eq!(out, vec![7], "out is unchanged");

        let got = decompress_with_limit(&mut out, compressed, CompressionType::LZ4, data.len());
        assert_eq!(got.unwrap(), Some(data.len()));
        assert_eq!(out.len(), data.len() + 1);

        let plain = Bytes::from_static(b"abc");
        let mut out = Vec::new();
        assert_eq!(
            decompress_with_limit(&mut out, plain.clone(), CompressionType::None, 2).unwrap(),
            None
        );
        assert_eq!(
            decompress_with_limit(&mut out, plain, CompressionType::None, 3).unwrap(),
            Some(3)
        );
    }
}
