use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};

use crate::meta::{Reorientation, exif_for_output, tiff_exif, xmp_for_output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Png,
    Jpeg,
    Heic,
    Webp,
}

pub const UNSUPPORTED_INPUT: &str = "不支持的输入格式（仅支持 PNG、JPEG、HEIC、WebP）";

/// 解码后的 8-bit RGBA 像素，方向已应用到像素上。
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub icc: Option<Vec<u8>>,
    /// 裸 TIFF 格式的 EXIF，方向和尺寸已改成输出后的值
    pub exif: Option<Vec<u8>>,
    /// XMP 包的 XML，方向和尺寸已改成输出后的值
    pub xmp: Option<Vec<u8>>,
}

impl Decoded {
    /// `regions` 是把 XMP 里的 MWG 区域从记录时的坐标系换到输出像素的变换
    fn new(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        icc: Option<Vec<u8>>,
        exif: Option<&[u8]>,
        xmp: Option<&[u8]>,
        regions: Reorientation,
    ) -> Self {
        Self {
            width,
            height,
            rgba,
            icc,
            exif: exif.and_then(|raw| exif_for_output(raw, width, height)),
            xmp: xmp.and_then(|raw| xmp_for_output(raw, width, height, regions)),
        }
    }
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
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some(InputFormat::Webp);
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
        Some(InputFormat::Webp) => decode_webp(data),
        None => bail!("{UNSUPPORTED_INPUT}"),
    }
}

fn decode_with_image(data: &[u8], image_format: ImageFormat) -> Result<Decoded> {
    let mut decoder = ImageReader::with_format(Cursor::new(data), image_format)
        .into_decoder()
        .context("解析图片头失败")?;
    let icc = decoder.icc_profile().context("读取 ICC 失败")?;
    let exif = decoder.exif_metadata().context("读取 EXIF 失败")?;
    let xmp = decoder.xmp_metadata().context("读取 XMP 失败")?;
    let orientation = decoder.orientation().context("读取 EXIF 方向失败")?;
    let mut img = DynamicImage::from_decoder(decoder).context("解码失败")?;
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    Ok(Decoded::new(
        rgba.width(),
        rgba.height(),
        rgba.into_raw(),
        icc,
        exif.as_deref(),
        xmp.as_deref(),
        Reorientation::from_exif(orientation.to_exif()),
    ))
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
    let exif = config
        .extract_exif(data)
        .map_err(|e| anyhow!("读取 HEIC 的 EXIF 失败：{e}"))?;
    let xmp = config
        .extract_xmp(data)
        .map_err(|e| anyhow!("读取 HEIC 的 XMP 失败：{e}"))?;
    // iPhone 的 HEIC 可能已按竖图存储、不带 irot，EXIF 方向却仍记着相对传感器的旋转，MWG 区域也按传感器方向写，
    // 所以区域优先按 EXIF 方向换算，EXIF 没写方向时才按 irot/imir
    let regions = exif
        .as_deref()
        .and_then(tiff_exif)
        .and_then(Orientation::from_exif_chunk)
        .filter(|&o| o != Orientation::NoTransforms)
        .map_or_else(
            || heic_reorientation(data),
            |o| Reorientation::from_exif(o.to_exif()),
        );
    Ok(Decoded::new(
        out.width,
        out.height,
        out.data,
        icc,
        exif.as_deref(),
        xmp.as_deref(),
        regions,
    ))
}

/// heic 解码时按 ipma 的顺序应用 imir、irot，这里按同样的顺序组合出整体变换。
/// irot 已被换算成顺时针角度；imir 的 axis 0 上下翻转、1 左右翻转，与 heic 的实现一致。
fn heic_reorientation(data: &[u8]) -> Reorientation {
    use heic::heif::Transform;
    let item = heic::heif::parse(data, &heic::Unstoppable)
        .ok()
        .and_then(|container| container.primary_item());
    let Some(item) = item else {
        return Reorientation::default();
    };
    item.transforms
        .iter()
        .fold(Reorientation::default(), |acc, t| {
            let step = match t {
                Transform::Mirror(mirror) if mirror.axis == 0 => Reorientation::from_exif(4),
                Transform::Mirror(_) => Reorientation::from_exif(2),
                Transform::Rotation(rotation) => Reorientation::from_exif(match rotation.angle {
                    90 => 6,
                    180 => 3,
                    270 => 8,
                    _ => 1,
                }),
                Transform::CleanAperture(_) => Reorientation::default(),
            };
            acc.then(step)
        })
}

fn decode_webp(data: &[u8]) -> Result<Decoded> {
    let info = zenwebp::ImageInfo::from_webp(data).map_err(|e| anyhow!("WebP 解析失败：{e}"))?;
    if info.has_animation {
        bail!("不支持动图 WebP");
    }
    let (rgba, width, height) =
        zenwebp::oneshot::decode_rgba(data).map_err(|e| anyhow!("WebP 解码失败：{e}"))?;
    // WebP 解码不会应用 EXIF 方向。规范要求 EXIF 是裸 TIFF，但有的写入方会带上 JPEG 式前缀
    let orientation = info
        .exif
        .as_deref()
        .and_then(tiff_exif)
        .and_then(Orientation::from_exif_chunk)
        .unwrap_or(Orientation::NoTransforms);
    let rgba = RgbaImage::from_raw(width, height, rgba).context("WebP 像素数据长度不符")?;
    let mut img = DynamicImage::ImageRgba8(rgba);
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    Ok(Decoded::new(
        rgba.width(),
        rgba.height(),
        rgba.into_raw(),
        info.icc_profile,
        info.exif.as_deref(),
        info.xmp.as_deref(),
        Reorientation::from_exif(orientation.to_exif()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::ROTATED_TIFF;

    #[test]
    fn sniff_formats() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some(InputFormat::Png));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(InputFormat::Jpeg));
        let heic = b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00mif1heic";
        assert_eq!(sniff(heic), Some(InputFormat::Heic));
        let avif = b"\x00\x00\x00\x18ftypavif\x00\x00\x00\x00mif1avif";
        assert_eq!(sniff(avif), None);
        assert_eq!(
            sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(InputFormat::Webp)
        );
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff(b"GIF89a"), None);
    }

    #[test]
    fn webp_applies_exif_orientation_with_or_without_prefix() {
        let tiff = ROTATED_TIFF;
        let prefixed = [&b"Exif\0\0"[..], tiff].concat();
        let rgb = vec![200u8; 8 * 4 * 3];
        let config = zenwebp::LossyConfig::new();
        for exif in [tiff, &prefixed[..]] {
            let webp =
                zenwebp::EncodeRequest::lossy(&config, &rgb, zenwebp::PixelLayout::Rgb8, 8, 4)
                    .with_exif(exif)
                    .encode()
                    .unwrap();
            let out = decode(&webp).unwrap();
            assert_eq!((out.width, out.height), (4, 8));
        }
    }
}
