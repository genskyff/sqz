use anyhow::{Result, anyhow};
use zenjpeg::encoder::{ChromaSubsampling, EncoderConfig, Exif, PixelLayout, Quality};

use crate::cli::{Chroma, Rgb};
use crate::decode::{icc_matches, is_gray, is_opaque};
use crate::meta::Metadata;

/// 一个 APP1 段能装下的 EXIF、XMP 上限。超出时编码器会直接截断，截断的数据是坏的，宁可不写
const MAX_EXIF_BYTES: usize = 65527;
const MAX_XMP_BYTES: usize = 65504;

/// 准备好的 JPEG 编码输入，可按不同质量重复编码。
pub struct JpegJob<'a> {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    layout: PixelLayout,
    gray: bool,
    chroma: ChromaSubsampling,
    meta: Metadata<'a>,
}

impl<'a> JpegJob<'a> {
    pub fn new(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        chroma: Chroma,
        background: Rgb,
        meta: Metadata<'a>,
    ) -> Self {
        let opaque = is_opaque(&rgba);
        let gray = opaque && meta.icc.is_none_or(|p| icc_matches(p, true)) && is_gray(&rgba);
        let (pixels, layout) = if gray {
            let luma = rgba.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
            (luma, PixelLayout::Gray8Srgb)
        } else if opaque {
            (rgba, PixelLayout::Rgba8Srgb)
        } else {
            (flatten(&rgba, background.0), PixelLayout::Rgb8Srgb)
        };
        Self {
            width,
            height,
            pixels,
            layout,
            gray,
            chroma: match chroma {
                Chroma::C420 => ChromaSubsampling::Quarter,
                Chroma::C444 => ChromaSubsampling::None,
            },
            meta: Metadata {
                icc: meta.icc.filter(|p| icc_matches(p, gray)),
                exif: meta.exif.filter(|e| e.len() <= MAX_EXIF_BYTES),
                xmp: meta.xmp.filter(|x| x.len() <= MAX_XMP_BYTES),
            },
        }
    }

    pub fn encode(&self, quality: u8) -> Result<Vec<u8>> {
        // 用 mozjpeg 刻度，-q 75 与 Squoosh / mozjpeg 的 q75 体积相近
        let q = Quality::ApproxMozjpeg(quality);
        let config = if self.gray {
            EncoderConfig::grayscale(q)
        } else {
            EncoderConfig::ycbcr(q, self.chroma)
        }
        // zenjpeg 默认会写 restart marker，部分解码器（如 Claude API）拒收带它的渐进式 JPEG
        .restart_mcu_rows(0);
        let mut request = config.request();
        if let Some(icc) = self.meta.icc {
            request = request.icc_profile(icc);
        }
        if let Some(exif) = self.meta.exif {
            request = request.exif(Exif::raw(exif));
        }
        if let Some(xmp) = self.meta.xmp {
            request = request.xmp(xmp);
        }
        request
            .encode_bytes(&self.pixels, self.width, self.height, self.layout)
            .map_err(|e| anyhow!("JPEG 编码失败：{e}"))
    }
}

/// 把 RGBA 按 alpha 合成到纯色底上，输出 RGB。
fn flatten(rgba: &[u8], bg: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for p in rgba.as_chunks::<4>().0 {
        let a = u32::from(p[3]);
        for c in 0..3 {
            let v = (u32::from(p[c]) * a + u32::from(bg[c]) * (255 - a) + 127) / 255;
            out.push(v as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;
    use crate::meta::{ROTATED_TIFF, ROTATED_XMP};

    #[test]
    fn flatten_blends_alpha() {
        let px = [255, 0, 0, 0, 255, 0, 0, 255, 0, 0, 255, 128];
        assert_eq!(
            flatten(&px, [255, 255, 255]),
            [255, 255, 255, 255, 0, 0, 127, 127, 255]
        );
    }

    #[test]
    fn writes_metadata_unless_too_large() {
        let rgba = [200, 100, 50, 255].repeat(8 * 4);
        let encode = |exif: &[u8], xmp: &[u8]| {
            let meta = Metadata {
                icc: None,
                exif: Some(exif),
                xmp: Some(xmp),
            };
            let job = JpegJob::new(8, 4, rgba.clone(), Chroma::C420, Rgb([255; 3]), meta);
            decode(&job.encode(80).unwrap()).unwrap()
        };
        // 读回时按写入的方向旋转了，说明 EXIF 确实写进去了
        let out = encode(ROTATED_TIFF, ROTATED_XMP);
        assert_eq!((out.width, out.height), (4, 8));
        assert!(out.exif.is_some());
        assert!(out.xmp.is_some());

        let mut huge_exif = ROTATED_TIFF.to_vec();
        huge_exif.resize(MAX_EXIF_BYTES + 1, 0);
        let mut huge_xmp = ROTATED_XMP.to_vec();
        huge_xmp.resize(MAX_XMP_BYTES + 1, b' ');
        let out = encode(&huge_exif, &huge_xmp);
        assert_eq!((out.width, out.height), (8, 4));
        assert!(out.exif.is_none());
        assert!(out.xmp.is_none());
    }
}
