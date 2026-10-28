use std::io::{BufReader, Read, Seek, SeekFrom};

use exr::block::reader::ChunksReader;
use exr::math::Vec2;
use exr::meta::BlockDescription;
use exr::meta::attribute::{LevelMode, Preview, SampleType};
use exr::prelude::f16;
use image::{DynamicImage, RgbaImage};
use tier1::ReadSeek;
use tier1::renderer::RenderOutput;

const TARGET_SIZE: usize = 512;

pub(crate) fn decode(mut source: Box<dyn ReadSeek + Send>) -> Option<RenderOutput> {
    source.seek(SeekFrom::Start(0)).ok()?;
    let reader = exr::block::read(BufReader::new(source), false).ok()?;
    let header = reader.headers().first()?;
    let display = header.shared_attributes.display_window;
    let width = u32::try_from(display.size.0).ok()?;
    let height = u32::try_from(display.size.1).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let mut properties = serde_json::json!({
        "width": width,
        "height": height,
    });
    if reader.headers().len() == 1 {
        let channels = &header.channels.list;
        let named = |name: &[u8]| channels.iter().any(|channel| channel.name.as_slice() == name);
        let simple = channels.iter().all(|channel| {
            matches!(channel.name.as_slice(), b"R" | b"G" | b"B" | b"Y" | b"A")
        }) && (named(b"R") && named(b"G") && named(b"B") || named(b"Y"));
        if simple {
            let bits: usize = channels.iter().filter(|channel| channel.name.as_slice() != b"A")
                .map(|channel| match channel.sample_type {
                    SampleType::F16 => 16,
                    SampleType::F32 | SampleType::U32 => 32,
                }).sum();
            properties["bpp"] = bits.into();
            properties["alpha"] = u8::from(named(b"A")).into();
        }
        properties["lossless"] = u8::from(channels.iter().all(|channel| {
            header.compression.is_lossless_for(channel.sample_type)
        })).into();
    }
    let same_window =
        display.position == header.own_attributes.layer_position && display.size == header.layer_size;
    let longest_side = u64::from(width.max(height));
    let target_side = longest_side.min(TARGET_SIZE as u64);

    if same_window
        && let Some(preview) = &header.own_attributes.preview
        && preview.size.0 as u64 >= (u64::from(width) * target_side).div_ceil(longest_side)
        && preview.size.1 as u64 >= (u64::from(height) * target_side).div_ceil(longest_side)
    {
        if let Some(image) = preview_image(preview) {
            return Some(output(image, properties, "preview"));
        }
    }

    let fallback_preview = header.own_attributes.preview.clone();
    let decoded = decode_pixels(reader, same_window, properties.clone());
    decoded.or_else(|| {
        let image = preview_image(fallback_preview.as_ref()?)?;
        Some(output(image, properties, "preview"))
    })
}

fn decode_pixels<R: Read + Seek>(
    reader: exr::block::reader::Reader<R>,
    same_window: bool,
    properties: serde_json::Value,
) -> Option<RenderOutput> {
    let header = reader.headers().first()?;
    let display = header.shared_attributes.display_window;
    if header.deep || header.channels.list.iter().any(|channel| channel.sampling != Vec2(1, 1)) {
        return None;
    }
    let channels = &header.channels.list;
    let has_channel = |name: &[u8]| channels.iter().any(|channel| channel.name.as_slice() == name);
    let prefix = if has_channel(b"R") && has_channel(b"G") && has_channel(b"B") || has_channel(b"Y") {
        &b""[..]
    } else {
        channels.iter().filter_map(|channel| channel.name.as_slice().strip_suffix(b"R")).find(
            |prefix| {
                prefix.ends_with(b".")
                    && has_channel(&[&prefix[..], b"G"].concat())
                    && has_channel(&[&prefix[..], b"B"].concat())
            },
        )?
    };
    let find_channel = |name: &[u8]| {
        channels.iter().position(|channel| channel.name.as_slice() == [prefix, name].concat())
    };
    let rgb = [find_channel(b"R"), find_channel(b"G"), find_channel(b"B")];
    let gray = find_channel(b"Y");
    if rgb.iter().any(Option::is_none) && gray.is_none() {
        return None;
    }
    let alpha = find_channel(b"A");
    let (level, size) = match header.blocks {
        _ if !same_window => (Vec2(0, 0), header.layer_size),
        BlockDescription::Tiles(tiles) if tiles.level_mode == LevelMode::MipMap => {
            exr::meta::mip_map_levels(tiles.rounding_mode, header.layer_size)
                .min_by_key(|(_, size)| size.0.max(size.1).abs_diff(TARGET_SIZE))
                .map(|(level, size)| (Vec2(level, level), size))
                .unwrap_or((Vec2(0, 0), header.layer_size))
        }
        BlockDescription::Tiles(tiles) if tiles.level_mode == LevelMode::RipMap => {
            exr::meta::rip_map_levels(tiles.rounding_mode, header.layer_size)
                .filter(|(level, _)| level.0 == level.1)
                .min_by_key(|(_, size)| size.0.max(size.1).abs_diff(TARGET_SIZE))
                .unwrap_or((Vec2(0, 0), header.layer_size))
        }
        _ => (Vec2(0, 0), header.layer_size),
    };
    let canvas_size = if same_window { size } else { display.size };
    let offset_x = i64::from(header.own_attributes.layer_position.0) - i64::from(display.position.0);
    let offset_y = i64::from(header.own_attributes.layer_position.1) - i64::from(display.position.1);
    let pixel_count = canvas_size.0.checked_mul(canvas_size.1)?;
    if pixel_count == 0 || pixel_count > 100_000_000 {
        return None;
    }
    let byte_count = pixel_count.checked_mul(4)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(byte_count).ok()?;
    pixels.resize(byte_count, 0);
    let filtered = reader
        .filter_chunks(false, |_, _, block| {
            let left = offset_x + block.pixel_position.0 as i64;
            let top = offset_y + block.pixel_position.1 as i64;
            block.layer == 0
                && block.level == level
                && left < canvas_size.0 as i64
                && top < canvas_size.1 as i64
                && left + block.pixel_size.0 as i64 > 0
                && top + block.pixel_size.1 as i64 > 0
        })
        .ok()?;
    filtered
        .decompress_sequential(false, |metadata, block| {
            if block.index.layer != 0
                || block.index.level != level
                || block
                    .index
                    .pixel_position
                    .0
                    .checked_add(block.index.pixel_size.0)
                    .is_none_or(|end| end > size.0)
                || block
                    .index
                    .pixel_position
                    .1
                    .checked_add(block.index.pixel_size.1)
                    .is_none_or(|end| end > size.1)
            {
                return Err(exr::error::Error::Invalid("unexpected EXR block".into()));
            }
            let header = &metadata.headers[0];
            let alpha_samples =
                if let Some(alpha_channel) = alpha {
                    let count =
                        block.index.pixel_size.0.checked_mul(block.index.pixel_size.1).ok_or_else(
                            || exr::error::Error::Invalid("EXR block is too large".into()),
                        )?;
                    let mut values = Vec::new();
                    values.try_reserve_exact(count).map_err(|_| {
                        exr::error::Error::Invalid("EXR alpha allocation failed".into())
                    })?;
                    values.resize(count, 0.0_f32);
                    for line in block.lines(&header.channels) {
                        if line.location.channel != alpha_channel {
                            continue;
                        }
                        let samples: Vec<f32> = match header.channels.list[alpha_channel].sample_type {
                            SampleType::F16 => line
                                .read_samples::<f16>()
                                .map(|value| value.map(f16::to_f32))
                                .collect::<exr::error::Result<_>>()?,
                            SampleType::F32 => {
                                line.read_samples::<f32>().collect::<exr::error::Result<_>>()?
                            }
                            SampleType::U32 => line
                                .read_samples::<u32>()
                                .map(|value| value.map(|value| value as f32))
                                .collect::<exr::error::Result<_>>()?,
                        };
                        let row = line.location.position.1 - block.index.pixel_position.1;
                        let start = row * block.index.pixel_size.0 + line.location.position.0
                            - block.index.pixel_position.0;
                        values[start..start + samples.len()].copy_from_slice(&samples);
                    }
                    Some(values)
                } else {
                    None
                };
            if alpha.is_none() {
                let left = (offset_x + block.index.pixel_position.0 as i64).max(0) as usize;
                let top = (offset_y + block.index.pixel_position.1 as i64).max(0) as usize;
                let right = (offset_x
                    + (block.index.pixel_position.0 + block.index.pixel_size.0) as i64)
                    .min(canvas_size.0 as i64) as usize;
                let bottom = (offset_y
                    + (block.index.pixel_position.1 + block.index.pixel_size.1) as i64)
                    .min(canvas_size.1 as i64) as usize;
                for row in top..bottom {
                    for column in left..right {
                        pixels[(row * canvas_size.0 + column) * 4 + 3] = 255;
                    }
                }
            }
            for line in block.lines(&header.channels) {
                let channel = line.location.channel;
                let positions = if rgb.iter().all(Option::is_some) {
                    [
                        rgb[0] == Some(channel),
                        rgb[1] == Some(channel),
                        rgb[2] == Some(channel),
                        alpha == Some(channel),
                    ]
                } else {
                    [
                        gray == Some(channel),
                        gray == Some(channel),
                        gray == Some(channel),
                        alpha == Some(channel),
                    ]
                };
                if !positions.iter().any(|selected| *selected) {
                    continue;
                }
                let samples: Vec<f32> = match header.channels.list[channel].sample_type {
                    SampleType::F16 => line
                        .read_samples::<f16>()
                        .map(|value| value.map(f16::to_f32))
                        .collect::<exr::error::Result<_>>()?,
                    SampleType::F32 => line.read_samples::<f32>().collect::<exr::error::Result<_>>()?,
                    SampleType::U32 => line
                        .read_samples::<u32>()
                        .map(|value| value.map(|value| value as f32))
                        .collect::<exr::error::Result<_>>()?,
                };
                for (offset, sample) in samples.into_iter().enumerate() {
                    let x = offset_x + (line.location.position.0 + offset) as i64;
                    let y = offset_y + line.location.position.1 as i64;
                    if x < 0 || y < 0 || x >= canvas_size.0 as i64 || y >= canvas_size.1 as i64 {
                        continue;
                    }
                    let index = (y as usize * canvas_size.0 + x as usize) * 4;
                    for (component, selected) in positions.iter().enumerate() {
                        if *selected {
                            let value = if component == 3 {
                                sample
                            } else {
                                let linear = if let Some(alpha_samples) = &alpha_samples {
                                    let local_x = line.location.position.0 + offset
                                        - block.index.pixel_position.0;
                                    let local_y =
                                        line.location.position.1 - block.index.pixel_position.1;
                                    let opacity =
                                        alpha_samples[local_y * block.index.pixel_size.0 + local_x];
                                    if opacity > 0.0 { sample / opacity } else { 0.0 }
                                } else {
                                    sample
                                };
                                linear_to_srgb(linear)
                            };
                            pixels[index + component] = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
                        }
                    }
                }
            }
            Ok(())
        })
        .ok()?;
    let image = RgbaImage::from_raw(
        u32::try_from(canvas_size.0).ok()?,
        u32::try_from(canvas_size.1).ok()?,
        pixels,
    )?;
    Some(output(DynamicImage::ImageRgba8(image), properties, "pixels"))
}

fn preview_image(preview: &Preview) -> Option<DynamicImage> {
    let width = u32::try_from(preview.size.0).ok()?;
    let height = u32::try_from(preview.size.1).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let pixels = preview.pixel_data.iter().map(|value| *value as u8).collect();
    RgbaImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgba8)
}

fn linear_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn output(image: DynamicImage, properties: serde_json::Value, codec: &str) -> RenderOutput {
    RenderOutput {
        image,
        renderer: Some("exr".into()),
        codec: Some(codec.into()),
        video_seek_secs: None,
        properties: Some(properties),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use exr::math::RoundingMode;
    use exr::meta::attribute::Preview;
    use exr::prelude::*;

    #[test]
    fn converts_linear_samples_to_srgb() {
        for (linear, expected) in [
            (0.0, 0),
            (0.003_130_8, 10),
            (0.18, 118),
            (0.5, 188),
            (1.0, 255),
        ] {
            let actual = (linear_to_srgb(linear).clamp(0.0, 1.0) * 255.0).round() as u8;
            assert_eq!(actual, expected);
        }
    }

    struct CountingReader {
        source: Cursor<Vec<u8>>,
        bytes_read: Arc<AtomicUsize>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let count = self.source.read(buf)?;
            self.bytes_read.fetch_add(count, Ordering::Relaxed);
            Ok(count)
        }
    }

    impl Seek for CountingReader {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.source.seek(position)
        }
    }

    #[test]
    fn preview_takes_priority_over_pixels() {
        let mut attributes = LayerAttributes::named("first");
        attributes.preview = Some(Preview {
            size: Vec2(8, 8),
            pixel_data: [12, 34, 56, -1].repeat(64),
        });
        let layer = Layer::new(
            (8, 8),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [12, 34, 56, 255]);
        assert_eq!(result.properties.unwrap()["width"], 8);
        assert_eq!(result.codec.as_deref(), Some("preview"));
    }

    #[test]
    fn skips_undersized_preview() {
        let mut attributes = LayerAttributes::named("first");
        attributes.preview = Some(Preview {
            size: Vec2(1, 1),
            pixel_data: vec![0, 0, -1, -1],
        });
        let layer = Layer::new(
            (8, 8),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(result.codec.as_deref(), Some("pixels"));

        let mut attributes = LayerAttributes::named("thin preview");
        attributes.preview = Some(Preview {
            size: Vec2(8, 1),
            pixel_data: [0, 0, -1, -1].repeat(8),
        });
        let layer = Layer::new(
            (8, 8),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        assert_eq!(decode(Box::new(data)).unwrap().codec.as_deref(), Some("pixels"));
    }

    #[test]
    fn uses_small_preview_for_unsupported_channels() {
        let mut attributes = LayerAttributes::named("depth");
        attributes.preview = Some(Preview {
            size: Vec2(1, 1),
            pixel_data: vec![12, 34, 56, -1],
        });
        let channels = AnyChannels::sort(SmallVec::from_vec(vec![AnyChannel::new(
            "Z",
            FlatSamples::F32(vec![0.5; 64]),
        )]));
        let layer = Layer::new((8, 8), attributes, Encoding::FAST_LOSSLESS, channels);
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.codec.as_deref(), Some("preview"));
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [12, 34, 56, 255]);
        assert_eq!(result.properties.unwrap()["width"], 8);
    }

    #[test]
    fn unsupported_channels_without_preview_are_not_claimed() {
        let channels = AnyChannels::sort(SmallVec::from_vec(vec![AnyChannel::new(
            "Z",
            FlatSamples::F32(vec![0.5; 64]),
        )]));
        let layer =
            Layer::new((8, 8), LayerAttributes::named("depth"), Encoding::FAST_LOSSLESS, channels);
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        assert!(decode(Box::new(data)).is_none());
    }

    #[test]
    fn uses_small_preview_for_unsupported_compression() {
        let mut attributes = LayerAttributes::named("unsupported compression");
        attributes.preview = Some(Preview {
            size: Vec2(1, 1),
            pixel_data: vec![12, 34, 56, -1],
        });
        let layer = Layer::new(
            (8, 8),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let bytes = data.get_mut();
        let attribute = b"compression\0compression\0\x01\0\0\0";
        let position = bytes.windows(attribute.len()).position(|window| window == attribute).unwrap();
        bytes[position + attribute.len()] = 11;
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.codec.as_deref(), Some("preview"));
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [12, 34, 56, 255]);
    }

    #[test]
    fn uses_preview_for_unsupported_channels_with_different_windows() {
        let mut attributes = LayerAttributes::named("depth").with_position(Vec2(1, 1));
        attributes.preview = Some(Preview {
            size: Vec2(1, 1),
            pixel_data: vec![12, 34, 56, -1],
        });
        let channels = AnyChannels::sort(SmallVec::from_vec(vec![AnyChannel::new(
            "Z",
            FlatSamples::F32(vec![0.5; 4]),
        )]));
        let layer = Layer::new((2, 2), attributes, Encoding::FAST_LOSSLESS, channels);
        let display = IntegerBounds {
            position: Vec2(0, 0),
            size: Vec2(4, 4),
        };
        let mut data = Cursor::new(Vec::new());
        Image::empty(ImageAttributes::new(display))
            .with_layer(layer)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.codec.as_deref(), Some("preview"));
        assert_eq!(result.properties.unwrap()["width"], 4);
    }

    #[test]
    fn positions_cropped_data_inside_display_window() {
        let display = IntegerBounds {
            position: Vec2(-2, -3),
            size: Vec2(5, 4),
        };
        let mut attributes = LayerAttributes::named("cropped").with_position(Vec2(-1, -2));
        attributes.preview = Some(Preview {
            size: Vec2(1, 1),
            pixel_data: vec![0, 0, -1, -1],
        });
        let layer = Layer::new(
            (2, 2),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let mut data = Cursor::new(Vec::new());
        Image::empty(ImageAttributes::new(display))
            .with_layer(layer)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let result = decode(Box::new(data)).unwrap();
        let image = result.image.to_rgba8();
        assert_eq!(image.dimensions(), (5, 4));
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(image.get_pixel(1, 1).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(2, 2).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(3, 2).0, [0, 0, 0, 0]);
        assert_eq!(result.codec.as_deref(), Some("pixels"));
        assert_eq!(result.properties.unwrap()["width"], 5);
    }

    #[test]
    fn clips_data_outside_display_window() {
        let display = IntegerBounds {
            position: Vec2(-1, 1),
            size: Vec2(3, 2),
        };
        let attributes = LayerAttributes::named("overscan").with_position(Vec2(-2, 0));
        let layer = Layer::new(
            (4, 4),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|position: Vec2<usize>| {
                (
                    if position.0 == 1 { 1.0_f32 } else { 0.0_f32 },
                    if position.1 == 1 { 1.0_f32 } else { 0.0_f32 },
                    0.0_f32,
                )
            }),
        );
        let mut data = Cursor::new(Vec::new());
        Image::empty(ImageAttributes::new(display))
            .with_layer(layer)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let result = decode(Box::new(data)).unwrap();
        let image = result.image.to_rgba8();
        assert_eq!(image.dimensions(), (3, 2));
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 0, 255]);
        assert_eq!(image.get_pixel(2, 1).0, [0, 0, 0, 255]);
        assert_eq!(result.properties.unwrap()["height"], 2);
    }

    #[test]
    fn preserves_data_alpha_and_transparent_display_padding() {
        let display = IntegerBounds {
            position: Vec2(0, 0),
            size: Vec2(4, 3),
        };
        let attributes = LayerAttributes::named("alpha").with_position(Vec2(1, 1));
        let layer = Layer::new(
            (2, 1),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgba(|position: Vec2<usize>| {
                (0.25_f32, 0.0_f32, 0.0_f32, if position.0 == 0 { 0.5_f32 } else { 1.0_f32 })
            }),
        );
        let mut data = Cursor::new(Vec::new());
        Image::empty(ImageAttributes::new(display))
            .with_layer(layer)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let result = decode(Box::new(data)).unwrap();
        let properties = result.properties.unwrap();
        assert_eq!(properties["bpp"], 96);
        assert_eq!(properties["alpha"], 1);
        assert_eq!(properties["lossless"], 1);
        let image = result.image.to_rgba8();
        assert_eq!(image.get_pixel(0, 1).0, [0, 0, 0, 0]);
        assert_eq!(image.get_pixel(1, 1).0[0], 188);
        assert_eq!(image.get_pixel(1, 1).0[3], 128);
        assert_eq!(image.get_pixel(2, 1).0[0], 137);
        assert_eq!(image.get_pixel(2, 1).0[3], 255);
        assert_eq!(image.get_pixel(3, 1).0, [0, 0, 0, 0]);
    }

    #[test]
    fn unpremultiplies_alpha_across_blocks() {
        let layer = Layer::new(
            (160, 130),
            LayerAttributes::named("alpha blocks"),
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgba(|position: Vec2<usize>| {
                let alpha = if position.0 < 80 { 0.25_f32 } else { 0.5_f32 };
                (alpha * 0.5, 0.0_f32, 0.0_f32, alpha)
            }),
        );
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let image = decode(Box::new(data)).unwrap().image.to_rgba8();
        assert_eq!(image.get_pixel(1, 1).0, [188, 0, 0, 64]);
        assert_eq!(image.get_pixel(140, 129).0, [188, 0, 0, 128]);
    }

    #[test]
    fn entirely_disjoint_data_produces_an_empty_display() {
        let display = IntegerBounds {
            position: Vec2(-4, -4),
            size: Vec2(2, 2),
        };
        let attributes = LayerAttributes::named("disjoint").with_position(Vec2(1, 1));
        let layer = Layer::new(
            (2, 2),
            attributes,
            Encoding::FAST_LOSSLESS,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let mut data = Cursor::new(Vec::new());
        Image::empty(ImageAttributes::new(display))
            .with_layer(layer)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let image = decode(Box::new(data)).unwrap().image.to_rgba8();
        assert_eq!(image.dimensions(), (2, 2));
        assert!(image.pixels().all(|pixel| pixel.0 == [0, 0, 0, 0]));
    }

    #[test]
    fn selects_mip_closest_to_target() {
        let size = Vec2(1024, 1024);
        let levels: Vec<_> = exr::meta::mip_map_levels(RoundingMode::Up, size).collect();
        let channel = |name| {
            AnyChannel::new(
                name,
                Levels::Mip {
                    level_data: levels
                        .iter()
                        .map(|(index, size)| FlatSamples::F32(vec![*index as f32 / 10.0; size.area()]))
                        .collect(),
                    rounding_mode: RoundingMode::Up,
                },
            )
        };
        let channels = AnyChannels::sort(exr::prelude::SmallVec::from_vec(vec![
            channel("R"),
            channel("G"),
            channel("B"),
        ]));
        let layer = Layer::new(size, LayerAttributes::named("mips"), Encoding::FAST_LOSSLESS, channels);
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!((result.image.width(), result.image.height()), (512, 512));
        let pixel = result.image.to_rgba8().get_pixel(0, 0).0;
        assert_eq!(pixel[0], 89);

        let channels =
            AnyChannels::sort(SmallVec::from_vec(vec![channel("R"), channel("G"), channel("B")]));
        let layer = Layer::new(
            size,
            LayerAttributes::named("offset mips").with_position(Vec2(100, 100)),
            Encoding::FAST_LOSSLESS,
            channels,
        );
        let display = IntegerBounds {
            position: Vec2(0, 0),
            size: Vec2(1200, 1200),
        };
        let mut data = Cursor::new(Vec::new());
        Image::empty(ImageAttributes::new(display))
            .with_layer(layer)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let image = decode(Box::new(data)).unwrap().image.to_rgba8();
        assert_eq!(image.dimensions(), (1200, 1200));
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(image.get_pixel(100, 100).0, [0, 0, 0, 255]);

        let size = Vec2(800, 200);
        let levels: Vec<_> = exr::meta::mip_map_levels(RoundingMode::Up, size).collect();
        let channel = |name| {
            AnyChannel::new(
                name,
                Levels::Mip {
                    level_data: levels
                        .iter()
                        .map(|(_, size)| FlatSamples::F32(vec![0.5; size.area()]))
                        .collect(),
                    rounding_mode: RoundingMode::Up,
                },
            )
        };
        let channels =
            AnyChannels::sort(SmallVec::from_vec(vec![channel("R"), channel("G"), channel("B")]));
        let layer =
            Layer::new(size, LayerAttributes::named("wide mips"), Encoding::FAST_LOSSLESS, channels);
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let image = decode(Box::new(data)).unwrap().image;
        assert_eq!((image.width(), image.height()), (400, 100));
    }

    #[test]
    fn decodes_only_first_part() {
        let size = Vec2(256, 256);
        let first = Layer::new(
            size,
            LayerAttributes::named("first"),
            Encoding::UNCOMPRESSED,
            SpecificChannels::rgb(|_| (1.0_f32, 0.0_f32, 0.0_f32)),
        );
        let second = Layer::new(
            size,
            LayerAttributes::named("second"),
            Encoding::UNCOMPRESSED,
            SpecificChannels::rgb(|_| (0.0_f32, 0.0_f32, 1.0_f32)),
        );
        let attributes = ImageAttributes::new(IntegerBounds::from_dimensions(size));
        let mut data = Cursor::new(Vec::new());
        Image::empty(attributes)
            .with_layer(first)
            .with_layer(second)
            .write()
            .to_buffered(&mut data)
            .unwrap();
        let total = data.get_ref().len();
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let result = decode(Box::new(CountingReader {
            source: data,
            bytes_read: bytes_read.clone(),
        }))
        .unwrap();
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert!(bytes_read.load(Ordering::Relaxed) < total * 2 / 3);
    }

    #[test]
    fn decodes_prefixed_channels() {
        let channel = |name, value| AnyChannel::new(name, FlatSamples::F32(vec![value; 4]));
        let channels = AnyChannels::sort(SmallVec::from_vec(vec![
            channel("view.R", 0.0),
            channel("view.G", 1.0),
            channel("view.B", 0.0),
        ]));
        let layer =
            Layer::new((2, 2), LayerAttributes::named("view"), Encoding::FAST_LOSSLESS, channels);
        let mut data = Cursor::new(Vec::new());
        Image::from_layer(layer).write().to_buffered(&mut data).unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [0, 255, 0, 255]);
    }
}
