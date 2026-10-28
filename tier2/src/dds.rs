use std::io::{BufReader, Seek, SeekFrom};

use image::{DynamicImage, RgbaImage};
use tier1::ReadSeek;
use tier1::renderer::RenderOutput;

pub(crate) fn decode(mut source: Box<dyn ReadSeek + Send>) -> Option<RenderOutput> {
    source.seek(SeekFrom::Start(0)).ok()?;
    let mut decoder = dds::Decoder::new(BufReader::new(source)).ok()?;
    let original_size = decoder.main_size();
    let format = decoder.format();
    let mut properties = serde_json::json!({"width": original_size.width, "height": original_size.height});
    let (bpp, alpha, lossless) = format_properties(format);
    if let Some(bpp) = bpp {
        properties["bpp"] = bpp.into();
    }
    if let Some(alpha) = alpha {
        properties["alpha"] = u8::from(alpha).into();
    }
    if let Some(lossless) = lossless {
        properties["lossless"] = u8::from(lossless).into();
    }
    if original_size.is_empty() || original_size.pixels() > 100_000_000 {
        return None;
    }
    let levels = match decoder.layout() {
        dds::DataLayout::Texture(texture) => texture.mipmaps(),
        dds::DataLayout::TextureArray(array) => array.mipmaps(),
        dds::DataLayout::Volume(_) => 1,
    };
    let level = (0..levels).min_by_key(|level| {
        let size = original_size.get_mipmap(*level);
        size.width.max(size.height).abs_diff(512)
    })?;
    for _ in 0..level {
        decoder.skip_surface().ok()?;
    }
    let size = decoder.surface_info()?.size();
    let byte_count = usize::try_from(size.pixels().checked_mul(4)?).ok()?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(byte_count).ok()?;
    pixels.resize(byte_count, 0);
    let view = dds::ImageViewMut::new(&mut pixels, size, dds::ColorFormat::RGBA_U8)?;
    decoder.read_surface(view).ok()?;
    let image = RgbaImage::from_raw(size.width, size.height, pixels)?;
    Some(RenderOutput {
        image: DynamicImage::ImageRgba8(image),
        renderer: Some("dds".into()),
        codec: Some(format!("{:?}", format)),
        video_seek_secs: None,
        properties: Some(properties),
    })
}

fn format_properties(format: dds::Format) -> (Option<u32>, Option<bool>, Option<bool>) {
    use dds::Format::*;
    match format {
        R8G8B8_UNORM | B8G8R8_UNORM | B8G8R8X8_UNORM => (Some(24), Some(false), Some(true)),
        R8G8B8A8_UNORM | R8G8B8A8_SNORM | B8G8R8A8_UNORM => (Some(24), Some(true), Some(true)),
        B5G6R5_UNORM => (Some(16), Some(false), Some(true)),
        B5G5R5A1_UNORM => (Some(15), Some(true), Some(true)),
        B4G4R4A4_UNORM | A4B4G4R4_UNORM => (Some(12), Some(true), Some(true)),
        R8_UNORM | R8_SNORM => (Some(8), Some(false), Some(true)),
        R8G8_UNORM | R8G8_SNORM | R16_UNORM | R16_SNORM | R16_FLOAT => {
            (Some(16), Some(false), Some(true))
        }
        R16G16_UNORM | R16G16_SNORM | R16G16_FLOAT | R32_FLOAT | R11G11B10_FLOAT => {
            (Some(32), Some(false), Some(true))
        }
        R16G16B16A16_UNORM | R16G16B16A16_SNORM | R16G16B16A16_FLOAT => {
            (Some(48), Some(true), Some(true))
        }
        R10G10B10A2_UNORM | R10G10B10_XR_BIAS_A2_UNORM => (Some(30), Some(true), Some(true)),
        R32G32_FLOAT => (Some(64), Some(false), Some(true)),
        R32G32B32_FLOAT => (Some(96), Some(false), Some(true)),
        R32G32B32A32_FLOAT => (Some(96), Some(true), Some(true)),
        BC1_UNORM => (None, None, Some(false)),
        BC2_UNORM | BC2_UNORM_PREMULTIPLIED_ALPHA | BC3_UNORM
        | BC3_UNORM_PREMULTIPLIED_ALPHA | BC7_UNORM => (None, Some(true), Some(false)),
        BC4_UNORM | BC4_SNORM | BC5_UNORM | BC5_SNORM | BC6H_UF16 | BC6H_SF16
        | BC3_UNORM_RXGB | BC3_UNORM_NORMAL => (None, Some(false), Some(false)),
        _ => (None, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn decodes_rgba_texture() {
        let size = dds::Size::new(4, 4);
        let format = dds::Format::R8G8B8A8_UNORM;
        let header = dds::header::Header::new_image(size.width, size.height, format);
        let pixels = [12, 34, 56, 128].repeat(16);
        let mut data = Cursor::new(Vec::new());
        let mut encoder = dds::Encoder::new(&mut data, format, &header).unwrap();
        encoder
            .write_surface(dds::ImageView::new(&pixels, size, dds::ColorFormat::RGBA_U8).unwrap())
            .unwrap();
        encoder.finish().unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [12, 34, 56, 128]);
        let properties = result.properties.unwrap();
        assert_eq!(properties["width"], 4);
        assert_eq!(properties["bpp"], 24);
        assert_eq!(properties["alpha"], 1);
        assert_eq!(properties["lossless"], 1);
    }

    #[test]
    fn selects_closest_mipmap_and_keeps_original_dimensions() {
        let size = dds::Size::new(1024, 512);
        let format = dds::Format::BC1_UNORM;
        let header =
            dds::header::Header::new_image(size.width, size.height, format).with_mipmap_count(3);
        let mut data = Cursor::new(Vec::new());
        let mut encoder = dds::Encoder::new(&mut data, format, &header).unwrap();
        for level in 0..3 {
            let mip = size.get_mipmap(level);
            let pixels = [255, 0, 0, 255].repeat(mip.pixels() as usize);
            encoder
                .write_surface(dds::ImageView::new(&pixels, mip, dds::ColorFormat::RGBA_U8).unwrap())
                .unwrap();
        }
        encoder.finish().unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!((result.image.width(), result.image.height()), (512, 256));
        assert_eq!(result.properties.unwrap()["width"], 1024);
        assert!(result.image.to_rgba8().get_pixel(0, 0).0[0] > 240);
    }

    #[test]
    fn compressed_formats_omit_ambiguous_color_depth() {
        assert_eq!(format_properties(dds::Format::BC3_UNORM), (None, Some(true), Some(false)));
        assert_eq!(format_properties(dds::Format::BC1_UNORM), (None, None, Some(false)));
    }

    #[test]
    fn rejects_truncated_data() {
        assert!(decode(Box::new(Cursor::new(b"DDS ".to_vec()))).is_none());
    }

    #[test]
    fn selects_mipmap_from_first_cube_face() {
        let size = dds::Size::new(1024, 1024);
        let format = dds::Format::R8G8B8A8_UNORM;
        let header =
            dds::header::Header::new_cube_map(size.width, size.height, format).with_mipmap_count(2);
        let mut data = Cursor::new(Vec::new());
        let mut encoder = dds::Encoder::new(&mut data, format, &header).unwrap();
        for face in 0..6 {
            for level in 0..2 {
                let mip = size.get_mipmap(level);
                let pixels = [face * 30, level * 100, 0, 255].repeat(mip.pixels() as usize);
                encoder
                    .write_surface(
                        dds::ImageView::new(&pixels, mip, dds::ColorFormat::RGBA_U8).unwrap(),
                    )
                    .unwrap();
            }
        }
        encoder.finish().unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!((result.image.width(), result.image.height()), (512, 512));
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [0, 100, 0, 255]);
    }

    #[test]
    fn selects_first_volume_slice() {
        let size = dds::Size::new(4, 4);
        let format = dds::Format::R8G8B8A8_UNORM;
        let header =
            dds::header::Header::new_volume(size.width, size.height, 2, format).with_mipmap_count(2);
        let mut data = Cursor::new(Vec::new());
        let mut encoder = dds::Encoder::new(&mut data, format, &header).unwrap();
        for (mip, color) in [(size, 10), (size, 20), (size.get_mipmap(1), 30)] {
            let pixels = [color, 0, 0, 255].repeat(mip.pixels() as usize);
            encoder
                .write_surface(dds::ImageView::new(&pixels, mip, dds::ColorFormat::RGBA_U8).unwrap())
                .unwrap();
        }
        encoder.finish().unwrap();
        let result = decode(Box::new(data)).unwrap();
        assert_eq!((result.image.width(), result.image.height()), (4, 4));
        assert_eq!(result.image.to_rgba8().get_pixel(0, 0).0, [10, 0, 0, 255]);
    }
}
