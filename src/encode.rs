mod jpeg;
mod png;
mod webp;

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::cli::Cli;
use crate::decode::InputFormat;
use crate::meta::Metadata;
use jpeg::JpegJob;
pub use png::Effort;
use png::PngJob;
use webp::WebpJob;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Jpeg,
    Png,
    Webp,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Png => "png",
            OutputFormat::Webp => "webp",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(OutputFormat::Jpeg),
            "png" => Some(OutputFormat::Png),
            "webp" => Some(OutputFormat::Webp),
            _ => None,
        }
    }

    pub fn same_as(self, input: InputFormat) -> bool {
        matches!(
            (self, input),
            (OutputFormat::Jpeg, InputFormat::Jpeg)
                | (OutputFormat::Png, InputFormat::Png)
                | (OutputFormat::Webp, InputFormat::Webp)
        )
    }

    /// 有损格式超过 95 体积暴涨而肉眼难辨
    pub fn size_mode_ceiling(self) -> u8 {
        match self {
            OutputFormat::Jpeg | OutputFormat::Webp => 95,
            OutputFormat::Png => 100,
        }
    }

    pub fn size_mode_floor(self) -> u8 {
        match self {
            OutputFormat::Jpeg | OutputFormat::Webp => 1,
            OutputFormat::Png => 30,
        }
    }
}

pub enum Encoder<'a> {
    Jpeg(JpegJob<'a>),
    Png(PngJob<'a>),
    Webp(WebpJob<'a>),
}

#[cfg_attr(feature = "hotpath", hotpath::measure_all)]
impl<'a> Encoder<'a> {
    pub fn new(
        format: OutputFormat,
        cli: &Cli,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        meta: Metadata<'a>,
    ) -> Self {
        match format {
            OutputFormat::Jpeg => Encoder::Jpeg(JpegJob::new(
                width,
                height,
                rgba,
                cli.chroma,
                cli.background,
                meta,
            )),
            OutputFormat::Png => Encoder::Png(PngJob::new(
                width,
                height,
                rgba,
                cli.colors,
                cli.dither.unwrap_or(1.0),
                meta,
            )),
            OutputFormat::Webp => Encoder::Webp(WebpJob::new(width, height, rgba, meta)),
        }
    }

    pub fn encode(&self, quality: u8, effort: Effort) -> Result<Vec<u8>> {
        match self {
            Encoder::Jpeg(job) => job.encode(quality),
            Encoder::Png(job) => job.encode(quality, effort),
            Encoder::Webp(job) => job.encode(quality),
        }
    }

    pub fn convert(&self) -> Result<Vec<u8>> {
        match self {
            Encoder::Jpeg(job) => job.encode(100),
            Encoder::Png(job) => job.encode(100, Effort::Final),
            Encoder::Webp(job) => job.lossless(),
        }
    }

    /// 无损搜索时已压到定稿，放得进 `cap` 就不重压
    pub fn refine(&self, quality: u8, searched: Vec<u8>, cap: u64) -> Result<Vec<u8>> {
        match self {
            Encoder::Png(_) if quality >= 100 && searched.len() as u64 <= cap => Ok(searched),
            Encoder::Png(job) => {
                let refined = job.encode(quality, Effort::Final)?;
                Ok(if refined.len() < searched.len() {
                    refined
                } else {
                    searched
                })
            }
            Encoder::Jpeg(_) | Encoder::Webp(_) => Ok(searched),
        }
    }

    pub fn dither(&self) -> Option<f32> {
        match self {
            Encoder::Jpeg(_) | Encoder::Webp(_) => None,
            Encoder::Png(job) => Some(job.dither()),
        }
    }

    pub fn drop_dither(&mut self) -> bool {
        match self {
            Encoder::Png(job) if job.dither() > 0.0 => {
                job.disable_dither();
                true
            }
            _ => false,
        }
    }
}

fn is_opaque(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
}

fn is_gray(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[0] == p[1] && p[1] == p[2])
}

/// ICC 头第 16-19 字节是颜色空间，和输出的颜色模型一致才能嵌入
fn icc_matches(icc: &[u8], gray: bool) -> bool {
    icc.get(16..20) == Some(if gray { &b"GRAY"[..] } else { &b"RGB "[..] })
}
