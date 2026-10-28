use std::io::BufReader;

use image::{DynamicImage, ImageDecoder, Limits};
use tier1::ReadSeek;

pub(crate) fn decode(reader: Box<dyn ReadSeek + Send>, ext: &str) -> Option<DynamicImage> {
    let reader = BufReader::new(reader);
    let limits = Limits::default();
    match ext {
        "icns" => decode_image(image_extras::icns::IcnsDecoder::new(reader).ok()?, limits),
        "ora" => decode_image(
            image_extras::ora::OpenRasterDecoder::with_limits(reader, limits.clone()).ok()?,
            limits,
        ),
        "otb" => decode_image(image_extras::otb::OtbDecoder::new(reader).ok()?, limits),
        "pcx" => decode_image(image_extras::pcx::PCXDecoder::new(reader).ok()?, limits),
        "sgi" => decode_image(image_extras::sgi::SgiDecoder::new(reader).ok()?, limits),
        "wbmp" => decode_image(image_extras::wbmp::WbmpDecoder::new(reader).ok()?, limits),
        "xbm" => decode_image(image_extras::xbm::XbmDecoder::new(reader).ok()?, limits),
        "xpm" => decode_image(image_extras::xpm::XpmDecoder::new(reader).ok()?, limits),
        _ => None,
    }
}

fn decode_image(mut decoder: impl ImageDecoder, limits: Limits) -> Option<DynamicImage> {
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 100_000_000 {
        return None;
    }
    if limits.max_alloc.is_some_and(|max| decoder.total_bytes() > max) {
        return None;
    }
    decoder.set_limits(limits).ok()?;
    DynamicImage::from_decoder(decoder).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, ImageEncoder};
    use std::io::{Cursor, Write};

    #[test]
    fn decodes_wbmp() {
        let bytes = vec![0, 0, 8, 1, 0x80];
        let image = decode(Box::new(Cursor::new(bytes)), "wbmp").unwrap();
        assert_eq!((image.width(), image.height()), (8, 1));
    }

    #[test]
    fn decodes_x_bitmap_formats() {
        let xbm = b"#define test_width 2\n#define test_height 1\nstatic unsigned char test_bits[] = { 0x01 };\n";
        let xpm = b"/* XPM */\nstatic char * test[] = {\n\"2 1 2 1\",\n\"a c #ff0000\",\n\"b c None\",\n\"ab\"\n};\n";
        for (ext, bytes) in [("xbm", xbm.as_slice()), ("xpm", xpm.as_slice())] {
            let image = decode(Box::new(Cursor::new(bytes.to_vec())), ext).unwrap();
            assert_eq!((image.width(), image.height()), (2, 1));
        }
    }

    #[test]
    fn rejects_invalid_images() {
        for ext in ["icns", "ora", "otb", "pcx", "sgi", "wbmp", "xbm", "xpm"] {
            assert!(decode(Box::new(Cursor::new(Vec::new())), ext).is_none(), "{ext}");
        }
    }

    #[test]
    fn decodes_otb_pcx_and_sgi() {
        let mut otb = Vec::new();
        image_extras::otb::OtbEncoder::new(&mut otb)
            .write_image(&[0, 255], 2, 1, ExtendedColorType::L8)
            .unwrap();

        let mut pcx = vec![0; 128];
        pcx[0..4].copy_from_slice(&[0x0a, 5, 1, 8]);
        pcx[8..10].copy_from_slice(&1u16.to_le_bytes());
        pcx[65] = 3;
        pcx[66..68].copy_from_slice(&2u16.to_le_bytes());
        pcx.extend_from_slice(&[40, 50, 60, 70, 80, 90]);

        let mut sgi = vec![0; 512];
        sgi[0..12].copy_from_slice(&[1, 0xda, 0, 1, 0, 3, 0, 2, 0, 1, 0, 3]);
        sgi[16..20].copy_from_slice(&255u32.to_be_bytes());
        sgi.extend_from_slice(&[40, 50, 60, 70, 80, 90]);

        for (ext, bytes) in [("otb", otb), ("pcx", pcx), ("sgi", sgi)] {
            let image = decode(Box::new(Cursor::new(bytes)), ext).unwrap();
            assert_eq!((image.width(), image.height()), (2, 1), "{ext}");
        }
    }

    #[test]
    fn decodes_icns_and_openraster() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[42, 100, 200, 255].repeat(16 * 16), 16, 16, ExtendedColorType::Rgba8)
            .unwrap();

        let mut icns = b"icns".to_vec();
        icns.extend_from_slice(&((png.len() + 16) as u32).to_be_bytes());
        icns.extend_from_slice(b"icp4");
        icns.extend_from_slice(&((png.len() + 8) as u32).to_be_bytes());
        icns.extend_from_slice(&png);

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("mimetype", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"image/openraster").unwrap();
        zip.start_file("mergedimage.png", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(&png).unwrap();
        let ora = zip.finish().unwrap().into_inner();

        for (ext, bytes) in [("icns", icns), ("ora", ora)] {
            let image = decode(Box::new(Cursor::new(bytes)), ext).unwrap();
            assert_eq!((image.width(), image.height()), (16, 16), "{ext}");
            assert_eq!(image.to_rgba8().get_pixel(0, 0).0, [42, 100, 200, 255], "{ext}");
        }
    }
}
