use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Png,
    Jpeg,
    Heic,
}

/// 解码后的 8-bit RGBA 像素，方向已应用到像素上。
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub icc: Option<Vec<u8>>,
}

pub fn is_opaque(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
}

pub fn is_gray(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[0] == p[1] && p[1] == p[2])
}

/// ICC 头部第 16-19 字节是颜色空间；只有和输出颜色模型一致的配置才能嵌入。
pub fn icc_matches(icc: &[u8], gray: bool) -> bool {
    icc.get(16..20) == Some(if gray { &b"GRAY"[..] } else { &b"RGB "[..] })
}

/// 按文件头识别格式，不看扩展名。
pub fn sniff(data: &[u8]) -> Option<InputFormat> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(InputFormat::Png);
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(InputFormat::Jpeg);
    }
    // ISO BMFF：[box size][ftyp][major brand][minor version][compatible brands...]
    if data.len() >= 16 && &data[4..8] == b"ftyp" {
        let box_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let end = box_len.clamp(16, data.len());
        let (fields, _) = data[8..end].as_chunks::<4>();
        // 第 1 个字段是 minor version，不是 brand
        let has_hevc_brand = fields.iter().enumerate().any(|(i, b)| {
            i != 1 && matches!(b, b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx")
        });
        if has_hevc_brand {
            return Some(InputFormat::Heic);
        }
    }
    None
}

pub fn decode(data: &[u8]) -> Result<Decoded> {
    match sniff(data) {
        Some(InputFormat::Png) => decode_with_image(data, ImageFormat::Png),
        Some(InputFormat::Jpeg) => decode_with_image(data, ImageFormat::Jpeg),
        Some(InputFormat::Heic) => decode_heic(data),
        None => bail!("不支持的输入格式（仅支持 PNG、JPEG、HEIC）"),
    }
}

fn decode_with_image(data: &[u8], image_format: ImageFormat) -> Result<Decoded> {
    let mut decoder = ImageReader::with_format(Cursor::new(data), image_format)
        .into_decoder()
        .context("解析图片头失败")?;
    let icc = decoder.icc_profile().context("读取 ICC 失败")?;
    let orientation = decoder.orientation().context("读取 EXIF 方向失败")?;
    let mut img = DynamicImage::from_decoder(decoder).context("解码失败")?;
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    Ok(Decoded {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
        icc,
    })
}

fn decode_heic(data: &[u8]) -> Result<Decoded> {
    // heic 会按 irot/imir 应用旋转镜像；HEIF 规范下 EXIF 里的方向不再生效
    let config = heic::DecoderConfig::new();
    let out = config
        .decode(data, heic::PixelLayout::Rgba8)
        .map_err(|e| anyhow!("HEIC 解码失败：{e}"))?;
    let icc = config
        .extract_icc(data)
        .map_err(|e| anyhow!("读取 HEIC 的 ICC 失败：{e}"))?;
    Ok(Decoded {
        width: out.width,
        height: out.height,
        rgba: out.data,
        icc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_formats() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some(InputFormat::Png));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(InputFormat::Jpeg));
        let heic = b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00mif1heic";
        assert_eq!(sniff(heic), Some(InputFormat::Heic));
        let avif = b"\x00\x00\x00\x18ftypavif\x00\x00\x00\x00mif1avif";
        assert_eq!(sniff(avif), None);
        assert_eq!(sniff(b"GIF89a"), None);
    }
}
