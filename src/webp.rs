use anyhow::{Result, anyhow};
use zenwebp::{EncodeRequest, LosslessConfig, LossyConfig, PixelLayout};

use crate::decode::{icc_matches, is_opaque};

/// 准备好的 WebP 编码输入，可按不同质量重复编码。
pub struct WebpJob<'a> {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    layout: PixelLayout,
    icc: Option<&'a [u8]>,
    exif: Option<&'a [u8]>,
}

impl<'a> WebpJob<'a> {
    pub fn new(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        icc: Option<&'a [u8]>,
        exif: Option<&'a [u8]>,
    ) -> Self {
        // 给了 alpha 通道编码器就会单独存一份，不透明的图只传 RGB
        let (pixels, layout) = if is_opaque(&rgba) {
            let rgb = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| &p[..3])
                .copied()
                .collect();
            (rgb, PixelLayout::Rgb8)
        } else {
            (rgba, PixelLayout::Rgba8)
        };
        Self {
            width,
            height,
            pixels,
            layout,
            icc: icc.filter(|p| icc_matches(p, false)),
            exif,
        }
    }

    pub fn encode(&self, quality: u8) -> Result<Vec<u8>> {
        let config = LossyConfig::new().with_quality(f32::from(quality));
        let request =
            EncodeRequest::lossy(&config, &self.pixels, self.layout, self.width, self.height);
        self.finish(request)
    }

    pub fn lossless(&self) -> Result<Vec<u8>> {
        let config = LosslessConfig::new();
        let request =
            EncodeRequest::lossless(&config, &self.pixels, self.layout, self.width, self.height);
        self.finish(request)
    }

    fn finish<'r>(&self, mut request: EncodeRequest<'r>) -> Result<Vec<u8>>
    where
        'a: 'r,
    {
        if let Some(icc) = self.icc {
            request = request.with_icc_profile(icc);
        }
        if let Some(exif) = self.exif {
            request = request.with_exif(exif);
        }
        request.encode().map_err(|e| anyhow!("WebP 编码失败：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{InputFormat, ROTATED_TIFF, decode, sniff};

    fn gradient(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                [
                    (x * 255 / width) as u8,
                    (y * 255 / height) as u8,
                    128,
                    alpha,
                ]
            })
            .collect()
    }

    #[test]
    fn roundtrip_keeps_size_and_alpha() {
        for alpha in [255, 128] {
            let src = gradient(48, 32, alpha);
            let webp = WebpJob::new(48, 32, src, None, None).encode(90).unwrap();
            assert_eq!(sniff(&webp), Some(InputFormat::Webp));
            let out = decode(&webp).unwrap();
            assert_eq!((out.width, out.height), (48, 32));
            assert!(
                out.rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p[3].abs_diff(alpha) <= 2)
            );
        }
    }

    #[test]
    fn lower_quality_is_smaller() {
        let src = gradient(64, 64, 255);
        let job = WebpJob::new(64, 64, src, None, None);
        assert!(job.encode(20).unwrap().len() < job.encode(95).unwrap().len());
    }

    #[test]
    fn lossless_keeps_pixels() {
        for alpha in [255, 128] {
            let src = gradient(48, 32, alpha);
            let webp = WebpJob::new(48, 32, src.clone(), None, None)
                .lossless()
                .unwrap();
            assert_eq!(decode(&webp).unwrap().rgba, src);
        }
    }

    #[test]
    fn writes_exif() {
        let job = WebpJob::new(8, 4, gradient(8, 4, 255), None, Some(ROTATED_TIFF));
        for webp in [job.encode(80).unwrap(), job.lossless().unwrap()] {
            // 读回时按写入的方向旋转了，说明 EXIF 确实写进去了
            let out = decode(&webp).unwrap();
            assert_eq!((out.width, out.height), (4, 8));
            assert!(out.exif.is_some());
        }
    }
}
