use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use rayon::prelude::*;

use super::{icc_matches, is_gray, is_opaque};
use crate::meta::Metadata;

/// 搜索时有损图用较快的 deflate 档位估算体积，定稿时再用最高档压一遍并取更小者。
/// 无损图估算不准，搜索时也压到定稿，超过 `cap` 就停。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Search { cap: u64 },
    Final,
}

/// 试探档位：用来比较滤波器和估算体积。更低的档位排序和最高档对不上，会挑错滤波器
const PROBE_LEVEL: u8 = 6;
const BEST_LEVEL: u8 = 9;

/// 压缩比达到这个倍数的数据，最高档还能小很多且压得快，总是值得再压一遍。经验值，样本有限。
const COMPRESSIBLE_RATIO: usize = 16;

/// 压缩比不到 `COMPRESSIBLE_RATIO` 时，试探结果超出上限这个倍数就不用最高档再压。经验值，样本有限。
const HOPELESS_MARGIN: f64 = 1.3;

/// 只收下 `limit` 字节，再多就报错，借此让压缩中途停下。
struct Capped {
    out: Vec<u8>,
    limit: usize,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.out.len() + buf.len() > self.limit {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// PNG 的 IDAT、iCCP 都要求 zlib 包装。输出一超过 `limit` 字节就中止，返回 None。
fn zlib_within(data: &[u8], level: u8, limit: usize) -> Option<Vec<u8>> {
    let sink = Capped {
        out: Vec::new(),
        limit,
    };
    let mut encoder = ZlibEncoder::new(sink, Compression::new(u32::from(level)));
    encoder.write_all(data).ok()?;
    Some(encoder.finish().ok()?.out)
}

fn zlib(data: &[u8], level: u8) -> Vec<u8> {
    zlib_within(data, level, usize::MAX).expect("不设上限就不会中止")
}

const COLOR_GRAY: u8 = 0;
const COLOR_RGB: u8 = 2;
const COLOR_INDEXED: u8 = 3;
const COLOR_GRAY_ALPHA: u8 = 4;
const COLOR_RGBA: u8 = 6;

/// 一次量化的结果，供定稿时复用：搜索最后试的质量往往就是要定稿的质量。
struct Quantized {
    quality: u8,
    dither: f32,
    palette: Vec<[u8; 4]>,
    indices: Vec<u8>,
}

/// 可跨线程共享，搜索时并行试探。
pub struct PngJob<'a> {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    quant_pixels: OnceLock<Vec<imagequant::RGBA>>,
    last_quantized: Mutex<Option<Arc<Quantized>>>,
    meta: Metadata<'a>,
    max_colors: u32,
    dither: f32,
}

impl<'a> PngJob<'a> {
    pub fn new(
        width: u32,
        height: u32,
        mut rgba: Vec<u8>,
        max_colors: u32,
        dither: f32,
        meta: Metadata<'a>,
    ) -> Self {
        // 全透明像素的 RGB 看不见，统一置零以利于压缩
        for p in rgba.as_chunks_mut::<4>().0 {
            if p[3] == 0 {
                p[..3].fill(0);
            }
        }
        Self {
            width,
            height,
            rgba,
            quant_pixels: OnceLock::new(),
            last_quantized: Mutex::new(None),
            meta,
            max_colors,
            dither,
        }
    }

    pub fn dither(&self) -> f32 {
        self.dither
    }

    pub fn disable_dither(&mut self) {
        self.dither = 0.0;
    }

    pub fn encode(&self, quality: u8, effort: Effort) -> Result<Vec<u8>> {
        if quality >= 100 {
            return Ok(self.lossless(effort));
        }
        // 抖动会在量化时烧进索引，缓存必须按（质量, 抖动）匹配。
        // 用 Arc 把结果带出锁，压缩期间不占锁，几个试探可以并行跑。
        let cached = {
            let last = self
                .last_quantized
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            last.as_ref()
                .filter(|q| q.quality == quality && q.dither == self.dither)
                .map(Arc::clone)
        };
        let quantized = match cached {
            Some(quantized) => quantized,
            None => {
                let quantized = Arc::new(self.quantize(quality)?);
                let mut last = self
                    .last_quantized
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *last = Some(Arc::clone(&quantized));
                quantized
            }
        };
        let raster = Raster::indexed(&quantized.palette, &quantized.indices, self.width);
        let best_within = match effort {
            Effort::Search { .. } => None,
            Effort::Final => Some(usize::MAX),
        };
        let idat = compress(&raster, best_within);
        Ok(assemble(&raster, self.width, self.height, &idat, self.meta))
    }

    fn quantize(&self, quality: u8) -> Result<Quantized> {
        let pixels = self.quant_pixels.get_or_init(|| {
            self.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&[r, g, b, a]| imagequant::RGBA::new(r, g, b, a))
                .collect()
        });
        let quant_err = |e: imagequant::Error| anyhow!("PNG 量化失败：{e}");
        let mut attr = imagequant::new();
        attr.set_max_colors(self.max_colors).map_err(quant_err)?;
        attr.set_quality(0, quality).map_err(quant_err)?;
        let mut image = attr
            .new_image_borrowed(pixels, self.width as usize, self.height as usize, 0.0)
            .map_err(quant_err)?;
        let mut result = attr.quantize(&mut image).map_err(quant_err)?;
        result.set_dithering_level(self.dither).map_err(quant_err)?;
        let (palette, indices) = result.remapped(&mut image).map_err(quant_err)?;
        Ok(Quantized {
            quality,
            dither: self.dither,
            palette: palette.iter().map(|c| [c.r, c.g, c.b, c.a]).collect(),
            indices,
        })
    }

    fn lossless(&self, effort: Effort) -> Vec<u8> {
        let opaque = is_opaque(&self.rgba);
        let gray = self.meta.icc.is_none_or(|icc| icc_matches(icc, true)) && is_gray(&self.rgba);

        let mut candidates = vec![Raster::truecolor(&self.rgba, self.width, gray, opaque)];
        if let Some((palette, indices)) = exact_palette(&self.rgba) {
            candidates.push(Raster::indexed(&palette, &indices, self.width));
        }
        let best_within = match effort {
            Effort::Search { cap } => usize::try_from(cap).unwrap_or(usize::MAX),
            Effort::Final => usize::MAX,
        };
        let (raster, idat) = candidates
            .into_par_iter()
            .map(|r| {
                let idat = compress(&r, Some(best_within));
                (r, idat)
            })
            .min_by_key(|(_, idat)| idat.len())
            .expect("至少有一个候选");
        assemble(&raster, self.width, self.height, &idat, self.meta)
    }
}

/// 一种 PNG 像素表示：未滤波的扫描线及其头信息。
struct Raster {
    color_type: u8,
    bit_depth: u8,
    /// 滤波时“左侧像素”的字节距离，不足 1 字节按 1 算
    filter_bpp: usize,
    row_bytes: usize,
    rows: Vec<u8>,
    plte: Vec<u8>,
    trns: Vec<u8>,
}

impl Raster {
    fn truecolor(rgba: &[u8], width: u32, gray: bool, opaque: bool) -> Self {
        let (color_type, channels): (u8, &[usize]) = match (gray, opaque) {
            (true, true) => (COLOR_GRAY, &[0]),
            (true, false) => (COLOR_GRAY_ALPHA, &[0, 3]),
            (false, true) => (COLOR_RGB, &[0, 1, 2]),
            (false, false) => (COLOR_RGBA, &[0, 1, 2, 3]),
        };
        let rows = if channels.len() == 4 {
            rgba.to_vec()
        } else {
            rgba.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| channels.iter().map(move |&c| p[c]))
                .collect()
        };
        Self {
            color_type,
            bit_depth: 8,
            filter_bpp: channels.len(),
            row_bytes: width as usize * channels.len(),
            rows,
            plte: Vec::new(),
            trns: Vec::new(),
        }
    }

    /// 调色板图：去掉未用到的颜色，半透明色排在前面以缩短 tRNS，并按颜色数选位深。
    fn indexed(palette: &[[u8; 4]], indices: &[u8], width: u32) -> Self {
        let mut used = [false; 256];
        for &i in indices {
            used[usize::from(i)] = true;
        }
        let mut order: Vec<usize> = (0..palette.len()).filter(|&i| used[i]).collect();
        order.sort_by_key(|&i| palette[i][3] == 255);
        let mut remap = [0u8; 256];
        for (new, &old) in order.iter().enumerate() {
            remap[old] = new as u8;
        }

        let bit_depth: u8 = match order.len() {
            0..=2 => 1,
            3..=4 => 2,
            5..=16 => 4,
            _ => 8,
        };
        let width = width as usize;
        let depth = usize::from(bit_depth);
        let row_bytes = (width * depth).div_ceil(8);
        let mut rows = vec![0u8; row_bytes * (indices.len() / width)];
        for (row_in, row_out) in indices
            .chunks_exact(width)
            .zip(rows.chunks_exact_mut(row_bytes))
        {
            for (x, &i) in row_in.iter().enumerate() {
                let bit = x * depth;
                row_out[bit / 8] |= remap[usize::from(i)] << (8 - depth - bit % 8);
            }
        }

        let plte = order
            .iter()
            .flat_map(|&i| palette[i][..3].to_vec())
            .collect();
        let trns = order
            .iter()
            .map(|&i| palette[i][3])
            .take_while(|&a| a != 255)
            .collect();
        Self {
            color_type: COLOR_INDEXED,
            bit_depth,
            filter_bpp: 1,
            row_bytes,
            rows,
            plte,
            trns,
        }
    }
}

fn exact_palette(rgba: &[u8]) -> Option<(Vec<[u8; 4]>, Vec<u8>)> {
    let mut lookup: HashMap<[u8; 4], u8> = HashMap::new();
    let mut palette = Vec::new();
    let mut indices = Vec::with_capacity(rgba.len() / 4);
    let mut last: Option<([u8; 4], u8)> = None;
    for &color in rgba.as_chunks::<4>().0 {
        let index = match last {
            Some((c, i)) if c == color => i,
            _ => match lookup.get(&color) {
                Some(&i) => i,
                None => {
                    if palette.len() == 256 {
                        return None;
                    }
                    let i = palette.len() as u8;
                    lookup.insert(color, i);
                    palette.push(color);
                    i
                }
            },
        };
        last = Some((color, index));
        indices.push(index);
    }
    Some((palette, indices))
}

#[derive(Debug, Clone, Copy)]
enum Filter {
    Fixed(u8),
    /// 每行选绝对值和最小的滤波器
    MinSum,
}

const ALL_FILTERS: [Filter; 6] = [
    Filter::Fixed(0),
    Filter::Fixed(1),
    Filter::Fixed(2),
    Filter::Fixed(3),
    Filter::Fixed(4),
    Filter::MinSum,
];
const INDEXED_FILTERS: [Filter; 2] = [Filter::Fixed(0), Filter::MinSum];

/// 试各滤波策略，返回最小的 zlib 数据。
///
/// `best_within` 为 None 时只按试探档位估算；否则再用最高档压一遍，结果不超过这个字节数
/// 且比试探结果小才采用。
fn compress(raster: &Raster, best_within: Option<usize>) -> Vec<u8> {
    let filters: &[Filter] = if raster.color_type == COLOR_INDEXED || raster.bit_depth < 8 {
        &INDEXED_FILTERS
    } else {
        &ALL_FILTERS
    };
    let (filtered, trial) = filters
        .par_iter()
        .map(|&f| {
            let filtered = apply_filter(raster, f);
            let trial = zlib(&filtered, PROBE_LEVEL);
            (filtered, trial)
        })
        .min_by_key(|(_, z)| z.len())
        .expect("至少有一个滤波策略");
    let Some(limit) = best_within else {
        return trial;
    };
    let compressible = trial.len() * COMPRESSIBLE_RATIO <= raster.rows.len();
    if !compressible && trial.len() as f64 > limit as f64 * HOPELESS_MARGIN {
        return trial;
    }
    let limit = limit.min(trial.len() - 1);
    zlib_within(&filtered, BEST_LEVEL, limit).unwrap_or(trial)
}

fn apply_filter(raster: &Raster, filter: Filter) -> Vec<u8> {
    let rb = raster.row_bytes;
    let bpp = raster.filter_bpp;
    let height = raster.rows.len() / rb;
    let zero = vec![0u8; rb];
    let mut scratch = vec![0u8; rb];
    let mut best = vec![0u8; rb];
    let mut out = Vec::with_capacity((rb + 1) * height);
    for y in 0..height {
        let row = &raster.rows[y * rb..(y + 1) * rb];
        let prev = if y == 0 {
            &zero[..]
        } else {
            &raster.rows[(y - 1) * rb..y * rb]
        };
        match filter {
            Filter::Fixed(t) => {
                filter_row(t, row, prev, bpp, &mut scratch);
                out.push(t);
                out.extend_from_slice(&scratch);
            }
            Filter::MinSum => {
                let mut best_type = 0;
                let mut best_score = u64::MAX;
                for t in 0..5 {
                    filter_row(t, row, prev, bpp, &mut scratch);
                    let score = scratch
                        .iter()
                        .map(|&b| u64::from((b as i8).unsigned_abs()))
                        .sum();
                    if score < best_score {
                        best_score = score;
                        best_type = t;
                        std::mem::swap(&mut best, &mut scratch);
                    }
                }
                out.push(best_type);
                out.extend_from_slice(&best);
            }
        }
    }
    out
}

fn filter_row(t: u8, row: &[u8], prev: &[u8], bpp: usize, out: &mut [u8]) {
    // 开头 bpp 个字节左边没有像素；分开处理，主循环没有分支才能向量化
    for i in 0..bpp {
        let predicted = match t {
            0 | 1 => 0,
            3 => prev[i] / 2,
            _ => prev[i],
        };
        out[i] = row[i].wrapping_sub(predicted);
    }
    // row、prev 从头对齐，就是左侧和左上的像素
    let body = out[bpp..]
        .iter_mut()
        .zip(&row[bpp..])
        .zip(&prev[bpp..])
        .zip(row)
        .zip(prev);
    match t {
        0 => body.for_each(|((((o, &x), _), _), _)| *o = x),
        1 => body.for_each(|((((o, &x), _), &left), _)| *o = x.wrapping_sub(left)),
        2 => body.for_each(|((((o, &x), &up), _), _)| *o = x.wrapping_sub(up)),
        3 => body.for_each(|((((o, &x), &up), &left), _)| {
            *o = x.wrapping_sub(((u16::from(left) + u16::from(up)) / 2) as u8)
        }),
        _ => body.for_each(|((((o, &x), &up), &left), &upper_left)| {
            *o = x.wrapping_sub(paeth(left, up, upper_left))
        }),
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn assemble(raster: &Raster, width: u32, height: u32, idat: &[u8], meta: Metadata) -> Vec<u8> {
    let mut out = Vec::with_capacity(idat.len() + raster.plte.len() + 1024);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // 位深、颜色类型、压缩方式、滤波方式、隔行
    ihdr.extend_from_slice(&[raster.bit_depth, raster.color_type, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);

    let gray = matches!(raster.color_type, COLOR_GRAY | COLOR_GRAY_ALPHA);
    if let Some(icc) = meta.icc.filter(|icc| icc_matches(icc, gray)) {
        // 配置名 + NUL + 压缩方式 0
        let mut iccp = b"ICC Profile\0\0".to_vec();
        iccp.extend_from_slice(&zlib(icc, BEST_LEVEL));
        write_chunk(&mut out, b"iCCP", &iccp);
    }
    if let Some(exif) = meta.exif {
        write_chunk(&mut out, b"eXIf", exif);
    }
    if let Some(xmp) = meta.xmp {
        // 关键字 + NUL + 不压缩 + 压缩方式 0 + 空语言标签 NUL + 空译名 NUL；XMP 规范要求不压缩
        let mut itxt = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
        itxt.extend_from_slice(xmp);
        write_chunk(&mut out, b"iTXt", &itxt);
    }
    if !raster.plte.is_empty() {
        write_chunk(&mut out, b"PLTE", &raster.plte);
    }
    if !raster.trns.is_empty() {
        write_chunk(&mut out, b"tRNS", &raster.trns);
    }
    write_chunk(&mut out, b"IDAT", idat);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;
    use crate::meta::{ROTATED_TIFF, ROTATED_XMP};

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                [
                    (x * 255 / width) as u8,
                    (y * 255 / height) as u8,
                    ((x + y) % 256) as u8,
                    255,
                ]
            })
            .collect()
    }

    #[test]
    fn lossless_roundtrip_truecolor() {
        let src = gradient(37, 23);
        let job = PngJob::new(37, 23, src.clone(), 256, 1.0, Metadata::default());
        for effort in [Effort::Search { cap: u64::MAX }, Effort::Final] {
            let png = job.encode(100, effort).unwrap();
            assert_eq!(decode(&png).unwrap().rgba, src);
        }
    }

    #[test]
    fn lossless_roundtrip_palette_with_alpha() {
        // 3 种颜色（含半透明）→ 2 位调色板，宽度不是 4 的倍数以覆盖行尾填充
        let colors = [[255, 0, 0, 255], [0, 0, 255, 128], [0, 0, 0, 0]];
        let src: Vec<u8> = (0..13 * 5).flat_map(|i| colors[i % 3]).collect();
        let png = PngJob::new(13, 5, src.clone(), 256, 1.0, Metadata::default())
            .encode(100, Effort::Final)
            .unwrap();
        assert_eq!(decode(&png).unwrap().rgba, src);
    }

    /// 16×16 的随机色块平铺：试探档位压出来比最高档大四成以上
    fn tiles(width: u32, height: u32) -> Vec<u8> {
        let mut state = 5u32;
        let tile: Vec<u8> = (0..16 * 16)
            .map(|_| {
                state = state.wrapping_mul(1103515245).wrapping_add(12345) & 0x7fff_ffff;
                ((state >> 16) % 8) as u8 * 36
            })
            .collect();
        (0..width * height)
            .flat_map(|i| {
                let v = tile[((i / width % 16) * 16 + i % width % 16) as usize];
                [v, 255 - v, v / 2, 255]
            })
            .collect()
    }

    #[test]
    fn lossless_search_settles_within_cap() {
        let job = PngJob::new(480, 360, tiles(480, 360), 256, 1.0, Metadata::default());
        let best = job.encode(100, Effort::Final).unwrap();
        // 上限为 0 时最高档立刻中止，得到试探结果；它比定稿大出 HOPELESS_MARGIN 以上，
        // 下面的结论才不是靠余量得来的
        let estimate = job.encode(100, Effort::Search { cap: 0 }).unwrap();
        assert!(estimate.len() as f64 > best.len() as f64 * HOPELESS_MARGIN);

        let cap = best.len() as u64;
        assert_eq!(job.encode(100, Effort::Search { cap }).unwrap(), best);
        let over = job.encode(100, Effort::Search { cap: cap - 1 }).unwrap();
        assert!(over.len() as u64 > cap - 1);
    }

    #[test]
    fn lossy_respects_max_colors() {
        let src = gradient(64, 64);
        let png = PngJob::new(64, 64, src, 16, 0.0, Metadata::default())
            .encode(80, Effort::Final)
            .unwrap();
        let out = decode(&png).unwrap();
        let distinct: std::collections::HashSet<_> = out.rgba.as_chunks::<4>().0.iter().collect();
        assert!(distinct.len() <= 16);
    }

    #[test]
    fn dither_off_recomputes_quantization() {
        // 抖动烧进了索引，关掉抖动后不能复用之前的量化结果
        let src = gradient(64, 64);
        let mut job = PngJob::new(64, 64, src.clone(), 256, 1.0, Metadata::default());
        job.encode(50, Effort::Final).unwrap();
        job.disable_dither();
        let after = job.encode(50, Effort::Final).unwrap();
        let fresh = PngJob::new(64, 64, src, 256, 0.0, Metadata::default())
            .encode(50, Effort::Final)
            .unwrap();
        assert_eq!(after, fresh);
    }

    #[test]
    fn writes_metadata() {
        let meta = Metadata {
            icc: None,
            exif: Some(ROTATED_TIFF),
            xmp: Some(ROTATED_XMP),
        };
        let job = PngJob::new(8, 4, gradient(8, 4), 256, 1.0, meta);
        for quality in [100, 50] {
            // 读回时按写入的方向旋转了，说明 EXIF 确实写进去了
            let out = decode(&job.encode(quality, Effort::Final).unwrap()).unwrap();
            assert_eq!((out.width, out.height), (4, 8));
            assert!(out.exif.is_some());
            assert!(out.xmp.is_some());
        }
    }
}
