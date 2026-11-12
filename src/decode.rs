use std::f32::consts::PI;
use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};
use rayon::prelude::*;

use crate::meta::{
    Reorientation, exif_for_output, set_exif_dimensions, tiff_exif, xmp_for_output, xmp_for_resized,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Png,
    Jpeg,
    Heic,
    Webp,
}

pub const UNSUPPORTED_INPUT: &str = "不支持的输入格式，仅支持 PNG、JPEG、HEIC、WebP";

/// 8-bit RGBA，方向已应用到像素上；EXIF 是裸 TIFF，XMP 是 XML，方向和尺寸都已按输出改好
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
}

impl Decoded {
    /// `regions`：MWG 区域从记录时的坐标系到输出像素的变换
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

    /// 长边超过 `max_edge` 时等比缩小，元数据里的尺寸跟着改；返回缩小前的尺寸
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub fn shrink_to_edge(&mut self, max_edge: u32) -> Option<(u32, u32)> {
        let source = (self.width, self.height);
        let long = self.width.max(self.height);
        if long <= max_edge {
            return None;
        }
        let scaled = |v: u32| {
            let rounded =
                (u64::from(v) * u64::from(max_edge) + u64::from(long) / 2) / u64::from(long);
            (rounded as u32).max(1)
        };
        let (width, height) = (scaled(self.width), scaled(self.height));
        self.rgba = resample(&self.rgba, source, (width, height));
        (self.width, self.height) = (width, height);
        if let Some(exif) = &mut self.exif {
            set_exif_dimensions(exif, width, height);
        }
        let scale = f64::from(max_edge) / f64::from(long);
        self.xmp = self
            .xmp
            .as_deref()
            .and_then(|xmp| xmp_for_resized(xmp, width, height, scale));
        Some(source)
    }
}

/// Lanczos3 缩小，先横后竖，逐行并行；按预乘 alpha 加权，否则透明像素的颜色会渗进边缘
fn resample(rgba: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (width, row, new_row) = (from.0 as usize, from.0 as usize * 4, to.0 as usize * 4);
    let columns = lanczos3_weights(from.0, to.0);
    let rows = lanczos3_weights(from.1, to.1);

    let mut narrowed = vec![0f32; new_row * from.1 as usize];
    narrowed
        .par_chunks_mut(new_row)
        .zip(rgba.par_chunks(row))
        .for_each_init(
            || vec![[0f32; 4]; width],
            |line, (out, src)| {
                for (p, s) in line.iter_mut().zip(src.as_chunks::<4>().0) {
                    let a = f32::from(s[3]);
                    *p = [
                        f32::from(s[0]) * a,
                        f32::from(s[1]) * a,
                        f32::from(s[2]) * a,
                        a,
                    ];
                }
                for (o, (left, weights)) in out.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
                    let mut sum = [0f32; 4];
                    for (p, w) in line[*left..].iter().zip(weights) {
                        for c in 0..4 {
                            sum[c] += p[c] * w;
                        }
                    }
                    *o = sum;
                }
            },
        );

    let mut out = vec![0u8; new_row * to.1 as usize];
    out.par_chunks_mut(new_row).zip(&rows).for_each_init(
        || vec![0f32; new_row],
        |sums, (out, (top, weights))| {
            sums.fill(0.0);
            let lines = narrowed[top * new_row..].chunks_exact(new_row);
            for (line, w) in lines.zip(weights) {
                for (sum, v) in sums.iter_mut().zip(line) {
                    *sum += v * w;
                }
            }
            let pixels = out.as_chunks_mut::<4>().0.iter_mut();
            for (o, p) in pixels.zip(sums.as_chunks::<4>().0) {
                let a = p[3];
                // 加 0.5 再截断即四舍五入，越界的值由 as 截到 0-255；全透明的像素留 0
                if a >= 0.5 {
                    let color = |c: f32| (c / a + 0.5) as u8;
                    *o = [color(p[0]), color(p[1]), color(p[2]), (a + 0.5) as u8];
                }
            }
        },
    );
    out
}

/// 每个输出像素的起始输入下标和归一化权重，只用于缩小；取样位置和窗口同 image 的 Lanczos3
fn lanczos3_weights(from: u32, to: u32) -> Vec<(usize, Vec<f32>)> {
    let sinc = |t: f32| {
        if t == 0.0 {
            1.0
        } else {
            (t * PI).sin() / (t * PI)
        }
    };
    let ratio = from as f32 / to as f32;
    let support = 3.0 * ratio;
    (0..to)
        .map(|i| {
            let center = (i as f32 + 0.5) * ratio;
            let left = ((center - support).floor() as usize).min(from as usize - 1);
            let right = ((center + support).ceil() as usize).clamp(left + 1, from as usize);
            let mut weights: Vec<f32> = (left..right)
                .map(|x| (x as f32 - (center - 0.5)) / ratio)
                .map(|t| {
                    if t.abs() < 3.0 {
                        sinc(t) * sinc(t / 3.0)
                    } else {
                        0.0
                    }
                })
                .collect();
            let sum: f32 = weights.iter().sum();
            weights.iter_mut().for_each(|w| *w /= sum);
            (left, weights)
        })
        .collect()
}

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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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
    // heic 已按 irot/imir 转好；按 HEIF 规范 EXIF 方向不生效
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
    // iPhone 的 HEIC 可能不带 irot，EXIF 方向和 MWG 区域却仍按传感器方向写：区域优先按 EXIF 方向换算
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

/// 按 heic 解码的顺序组合 imir、irot。irot 是顺时针角度；imir 的 axis 0 上下翻、1 左右翻
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
    // WebP 解码不应用 EXIF 方向；有的写入方给 EXIF 带上 JPEG 式前缀
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
    use image::imageops::{self, FilterType};

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

    fn bare(width: u32, height: u32, rgba: Vec<u8>) -> Decoded {
        Decoded {
            width,
            height,
            rgba,
            icc: None,
            exif: None,
            xmp: None,
        }
    }

    #[test]
    fn shrink_to_edge_scales_the_long_edge_down() {
        for ((width, height), max_edge, expected) in [
            ((80, 40), 40, Some((40, 20))),
            ((30, 90), 45, Some((15, 45))),
            // 短边四舍五入，至少留 1 像素
            ((100, 33), 50, Some((50, 17))),
            ((200, 1), 10, Some((10, 1))),
            // 不超过上限的原样不动，不放大
            ((80, 40), 80, None),
            ((80, 40), 200, None),
        ] {
            let source: Vec<u8> = (0..width * height)
                .flat_map(|i| [(i % width) as u8, (i / width) as u8, 128, 255])
                .collect();
            let mut img = bare(width, height, source.clone());
            assert_eq!(
                img.shrink_to_edge(max_edge),
                expected.map(|_| (width, height))
            );
            let (w, h) = expected.unwrap_or((width, height));
            assert_eq!((img.width, img.height), (w, h));
            assert_eq!(img.rgba.len(), (w * h * 4) as usize);
            assert!(img.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
            if expected.is_none() {
                assert_eq!(img.rgba, source);
            }
        }
    }

    /// image 的 Lanczos3，按预乘 alpha 缩放
    fn reference(rgba: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
        let img = RgbaImage::from_raw(from.0, from.1, rgba.to_vec()).unwrap();
        let mut img = DynamicImage::ImageRgba8(img).into_rgba32f();
        for p in img.pixels_mut() {
            let [r, g, b, a] = &mut p.0;
            for c in [r, g, b] {
                *c *= *a;
            }
        }
        let mut out = imageops::resize(&img, to.0, to.1, FilterType::Lanczos3);
        for p in out.pixels_mut() {
            let [r, g, b, a] = &mut p.0;
            for c in [r, g, b] {
                *c = (*c / *a).min(1.0);
            }
        }
        DynamicImage::ImageRgba32F(out).into_rgba8().into_raw()
    }

    #[test]
    fn resample_matches_image_lanczos3() {
        let mut state = 5u32;
        let mut noise = || {
            state = state.wrapping_mul(1103515245).wrapping_add(12345) & 0x7fff_ffff;
            (state >> 16) as u8
        };
        for (from, to) in [
            ((67, 41), (23, 14)),
            ((50, 37), (49, 36)),
            ((200, 3), (10, 1)),
        ] {
            // 一张不透明，一张 alpha 沿横向平缓变化
            for opaque in [true, false] {
                let rgba: Vec<u8> = (0..from.0 * from.1)
                    .flat_map(|i| {
                        let alpha = if opaque { 255 } else { 40 + i % from.0 % 200 };
                        [noise(), noise(), noise(), alpha as u8]
                    })
                    .collect();
                let diff = resample(&rgba, from, to)
                    .iter()
                    .zip(&reference(&rgba, from, to))
                    .map(|(a, b)| a.abs_diff(*b))
                    .max();
                assert!(diff <= Some(1), "{from:?} → {to:?} {diff:?}");
            }
        }
    }

    #[test]
    fn shrink_to_edge_keeps_colors_next_to_transparency() {
        // 左 7 列不透明、右 5 列透明黑：不按预乘 alpha 重采样，交界处会掺进黑色
        let rgba = (0..12 * 3)
            .flat_map(|i| {
                if i % 12 < 7 {
                    [200, 100, 50, 255]
                } else {
                    [0; 4]
                }
            })
            .collect();
        let mut img = bare(12, 3, rgba);
        assert_eq!(img.shrink_to_edge(4), Some((12, 3)));
        let pixels = img.rgba.as_chunks::<4>().0;
        assert!(pixels.iter().any(|p| (1..255).contains(&p[3])));
        assert!(
            pixels
                .iter()
                .filter(|p| p[3] > 0)
                .all(|p| p[..3] == [200, 100, 50])
        );
    }

    #[test]
    fn shrink_to_edge_measures_the_upright_image_and_updates_metadata() {
        // ImageWidth=80、ImageLength=40、Orientation=6：转正后是 40×80 的竖图
        const TIFF: &[u8] = b"II*\0\x08\0\0\0\x03\0\
            \0\x01\x03\0\x01\0\0\0\x50\0\0\0\
            \x01\x01\x03\0\x01\0\0\0\x28\0\0\0\
            \x12\x01\x03\0\x01\0\0\0\x06\0\0\0\
            \0\0\0\0";
        const XMP: &[u8] = br#"<rdf:Description tiff:Orientation="6" tiff:ImageWidth="80" tiff:ImageLength="40"/>"#;
        let rgb = vec![200u8; 80 * 40 * 3];
        let config = zenwebp::LossyConfig::new();
        let webp = zenwebp::EncodeRequest::lossy(&config, &rgb, zenwebp::PixelLayout::Rgb8, 80, 40)
            .with_exif(TIFF)
            .with_xmp(XMP)
            .encode()
            .unwrap();
        let mut out = decode(&webp).unwrap();
        assert_eq!(out.shrink_to_edge(40), Some((40, 80)));
        assert_eq!((out.width, out.height), (20, 40));

        let exif = out.exif.unwrap();
        assert_eq!(
            Orientation::from_exif_chunk(&exif),
            Some(Orientation::NoTransforms)
        );
        // 两个尺寸条目的值分别在第 18、30 字节
        assert_eq!((exif[18], exif[30]), (20, 40));
        assert_eq!(
            String::from_utf8(out.xmp.unwrap()).unwrap(),
            r#"<rdf:Description tiff:Orientation="1" tiff:ImageWidth="20" tiff:ImageLength="40"/>"#
        );
    }
}
