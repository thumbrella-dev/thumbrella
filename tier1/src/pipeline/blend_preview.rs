//! Extract the file-level preview from a bounded Blender file prefix.

use std::io::{self, Read};

use image::{DynamicImage, RgbaImage};

const MAX_DECODED: u64 = 2 * 1024 * 1024;
const MAX_WINDOW: u64 = 8 * 1024 * 1024;
const MAX_PIXELS: u64 = 512 * 512;
const MAX_BLOCKS: usize = 256;
const MAX_FRAMES: usize = 256;

pub(super) enum Preview {
    Image(DynamicImage),
    Absent,
    NeedMore,
}

pub(super) fn extract(data: &[u8]) -> Preview {
    if data.starts_with(b"\x1f\x8b") {
        return parse(flate2::read::MultiGzDecoder::new(data));
    }
    if is_zstd(data) {
        return parse(ZstdFrames {
            source: data,
            decoder: None,
            frames: 0,
        });
    }
    parse(data)
}

pub(super) fn is_zstd(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let magic = read_u32(&data[..4], false);
    magic == 0xfd2fb528 || (0x184d2a50..=0x184d2a5f).contains(&magic)
}

struct ZstdFrames<'a> {
    source: &'a [u8],
    decoder: Option<ruzstd::decoding::StreamingDecoder<&'a [u8], ruzstd::decoding::FrameDecoder>>,
    frames: usize,
}

impl Read for ZstdFrames<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.decoder.is_none() {
                if self.source.is_empty() {
                    return Ok(0);
                }
                if self.frames >= MAX_FRAMES {
                    return Err(io::Error::other("too many Zstandard frames"));
                }
                if self.source.len() >= 4
                    && (0x184d2a50..=0x184d2a5f).contains(&read_u32(&self.source[..4], false))
                {
                    if self.source.len() < 8 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    let size = read_u32(&self.source[4..8], false) as usize;
                    let end = 8usize.checked_add(size).ok_or(io::ErrorKind::InvalidData)?;
                    self.source = self.source.get(end..).ok_or(io::ErrorKind::UnexpectedEof)?;
                    self.frames += 1;
                    continue;
                }
                self.decoder = Some(
                    ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
                        self.source, MAX_WINDOW,
                    )
                    .map_err(io::Error::other)?,
                );
                self.frames += 1;
            }
            let decoder = self.decoder.as_mut().expect("decoder was initialized");
            let count = decoder.read(out)?;
            if count > 0 {
                return Ok(count);
            }
            self.source = self.decoder.take().expect("decoder was initialized").into_inner();
        }
    }
}

fn parse(reader: impl Read) -> Preview {
    match parse_inner(reader.take(MAX_DECODED)) {
        Ok(Some(img)) => Preview::Image(img),
        Ok(None) => Preview::Absent,
        Err(_) => Preview::NeedMore,
    }
}

fn parse_inner(mut reader: impl Read) -> io::Result<Option<DynamicImage>> {
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;
    if &header[..7] != b"BLENDER" {
        return Ok(None);
    }

    let (header_size, length_offset, large, big_endian) = if &header[7..12] == b"17-01" {
        let mut rest = [0; 5];
        reader.read_exact(&mut rest)?;
        if rest[0] != b'v' || !rest[1..].iter().all(u8::is_ascii_digit) {
            return Ok(None);
        }
        (32, 16, true, false)
    } else {
        let size = match header[7] {
            b'_' => 20,
            b'-' => 24,
            _ => return Ok(None),
        };
        let big_endian = match header[8] {
            b'v' => false,
            b'V' => true,
            _ => return Ok(None),
        };
        if !header[9..].iter().all(u8::is_ascii_digit) || &header[9..] < b"250".as_slice() {
            return Ok(None);
        }
        (size, 4, false, big_endian)
    };

    let mut remaining = MAX_DECODED - if large { 17 } else { 12 };
    for _ in 0..MAX_BLOCKS {
        if remaining < header_size as u64 {
            return Ok(None);
        }
        let mut block = [0; 32];
        reader.read_exact(&mut block[..header_size])?;
        remaining -= header_size as u64;
        let len = if large {
            read_u64(&block[length_offset..length_offset + 8], big_endian)
        } else {
            u64::from(read_u32(&block[length_offset..length_offset + 4], big_endian))
        };
        if len > remaining {
            return Ok(None);
        }

        match &block[..4] {
            b"REND" => {
                let mut left = len;
                let mut skip = [0; 4096];
                while left > 0 {
                    let n = left.min(skip.len() as u64) as usize;
                    reader.read_exact(&mut skip[..n])?;
                    left -= n as u64;
                }
                remaining -= len;
            }
            b"TEST" => {
                if len < 8 {
                    return Ok(None);
                }
                let mut shape = [0; 8];
                reader.read_exact(&mut shape)?;
                let width = read_u32(&shape[..4], big_endian);
                let height = read_u32(&shape[4..], big_endian);
                let pixels = u64::from(width) * u64::from(height);
                if width == 0 || height == 0 || pixels > MAX_PIXELS || len != 8 + pixels * 4 {
                    return Ok(None);
                }
                let mut rgba = vec![0; (pixels * 4) as usize];
                reader.read_exact(&mut rgba)?;
                let stride = width as usize * 4;
                for y in 0..height as usize / 2 {
                    let other = height as usize - 1 - y;
                    for x in 0..stride {
                        rgba.swap(y * stride + x, other * stride + x);
                    }
                }
                return Ok(RgbaImage::from_raw(width, height, rgba).map(DynamicImage::ImageRgba8));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn read_u32(bytes: &[u8], big_endian: bool) -> u32 {
    let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if big_endian { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) }
}

fn read_u64(bytes: &[u8], big_endian: bool) -> u64 {
    let bytes = [
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ];
    if big_endian { u64::from_be_bytes(bytes) } else { u64::from_le_bytes(bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(pointer_64: bool, big_endian: bool, modern: bool) -> Vec<u8> {
        let mut out = if modern {
            b"BLENDER17-01v0500".to_vec()
        } else {
            let mut h = b"BLENDER-v450".to_vec();
            h[7] = if pointer_64 { b'-' } else { b'_' };
            h[8] = if big_endian { b'V' } else { b'v' };
            h
        };
        let block_len = if modern {
            32
        } else if pointer_64 {
            24
        } else {
            20
        };
        let mut block = vec![0; block_len];
        block[..4].copy_from_slice(b"REND");
        let rend_len = if big_endian { 4u32.to_be_bytes() } else { 4u32.to_le_bytes() };
        if modern {
            block[16..24].copy_from_slice(&4u64.to_le_bytes());
        } else {
            block[4..8].copy_from_slice(&rend_len);
        }
        out.extend_from_slice(&block);
        out.extend_from_slice(b"test");
        block[..4].copy_from_slice(b"TEST");
        if modern {
            block[16..24].copy_from_slice(&24u64.to_le_bytes());
        } else {
            let len = if big_endian { 24u32.to_be_bytes() } else { 24u32.to_le_bytes() };
            block[4..8].copy_from_slice(&len);
        }
        out.extend_from_slice(&block);
        let dim = if big_endian { 2u32.to_be_bytes() } else { 2u32.to_le_bytes() };
        out.extend_from_slice(&dim);
        out.extend_from_slice(&dim);
        out.extend_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255]);
        out.extend_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]);
        out
    }

    fn assert_preview(data: &[u8]) {
        let Preview::Image(image) = extract(data) else {
            panic!("expected preview");
        };
        let image = image.to_rgba8();
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(0, 1).0, [255, 0, 0, 255]);
    }

    #[test]
    fn reads_legacy_and_modern_headers() {
        for (ptr64, big, modern) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert_preview(&fixture(ptr64, big, modern));
        }
    }

    #[test]
    fn reads_gzip_and_zstd() {
        let raw = fixture(true, false, false);
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&raw).unwrap();
        assert_preview(&gzip.finish().unwrap());
        let zstd = ruzstd::encoding::compress_to_vec(
            raw.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert_preview(&zstd);
        let mut frames =
            ruzstd::encoding::compress_to_vec(&raw[..20], ruzstd::encoding::CompressionLevel::Fastest);
        frames.extend_from_slice(&0x184d2a50u32.to_le_bytes());
        frames.extend_from_slice(&4u32.to_le_bytes());
        frames.extend_from_slice(b"skip");
        frames.extend(ruzstd::encoding::compress_to_vec(
            &raw[20..],
            ruzstd::encoding::CompressionLevel::Fastest,
        ));
        assert_preview(&frames);
        let mut leading_skip = 0x184d2a50u32.to_le_bytes().to_vec();
        leading_skip.extend_from_slice(&00u32.to_le_bytes());
        leading_skip.extend_from_slice(&frames);
        assert_preview(&leading_skip);
    }

    #[test]
    fn rejects_missing_invalid_and_oversized_previews() {
        let raw = fixture(true, false, false);
        assert!(matches!(extract(&raw[..raw.len() - 1]), Preview::NeedMore));
        let mut missing = raw.clone();
        missing[40..44].copy_from_slice(b"GLOB");
        assert!(matches!(extract(&missing), Preview::Absent));
        let mut huge = raw;
        let shape_offset = 12 + 24 + 4 + 24;
        huge[shape_offset..shape_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(extract(&huge), Preview::Absent));
    }
}
