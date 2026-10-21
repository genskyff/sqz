use std::iter::StepBy;
use std::ops::Range;

use image::metadata::Orientation;

/// 要写进输出的元数据，EXIF、XMP 已按输出调整好
#[derive(Debug, Clone, Copy, Default)]
pub struct Metadata<'a> {
    pub icc: Option<&'a [u8]>,
    /// 裸 TIFF 格式
    pub exif: Option<&'a [u8]>,
    /// XMP 包的 XML 文本
    pub xmp: Option<&'a [u8]>,
}

/// 统一成裸 TIFF：去掉 JPEG 式的 `Exif\0\0` 前缀，丢弃不是 TIFF 的数据。
pub fn tiff_exif(raw: &[u8]) -> Option<&[u8]> {
    let tiff = raw.strip_prefix(b"Exif\0\0").unwrap_or(raw);
    (tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*")).then_some(tiff)
}

/// 方向已应用到像素上：方向改成正常、尺寸改成输出尺寸，否则看图软件会再转一次、读到错的宽高。
pub fn exif_for_output(raw: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut exif = tiff_exif(raw)?.to_vec();
    let _ = Orientation::remove_from_exif_chunk(&mut exif);
    set_exif_dimensions(&mut exif, width, height);
    Some(exif)
}

/// 图像的旋转、镜像：先按需转置（x、y 对调），再按需左右、上下翻转。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reorientation {
    transpose: bool,
    flip_x: bool,
    flip_y: bool,
}

impl Reorientation {
    /// EXIF Orientation 1-8 表示的变换，其他值当作不变
    pub fn from_exif(value: u8) -> Self {
        let (transpose, flip_x, flip_y) = match value {
            2 => (false, true, false),
            3 => (false, true, true),
            4 => (false, false, true),
            5 => (true, false, false),
            6 => (true, true, false),
            7 => (true, true, true),
            8 => (true, false, true),
            _ => (false, false, false),
        };
        Self {
            transpose,
            flip_x,
            flip_y,
        }
    }

    /// 先做 self 再做 next
    pub fn then(self, next: Self) -> Self {
        let (flip_x, flip_y) = if next.transpose {
            (self.flip_y, self.flip_x)
        } else {
            (self.flip_x, self.flip_y)
        };
        Self {
            transpose: self.transpose ^ next.transpose,
            flip_x: flip_x ^ next.flip_x,
            flip_y: flip_y ^ next.flip_y,
        }
    }
}

/// XMP 里的方向和尺寸同 EXIF 一样改成输出后的值，MWG 区域按 `regions` 换到输出像素上；
/// 不是 UTF-8 的 XMP 无法改写，丢弃。
pub fn xmp_for_output(
    raw: &[u8],
    width: u32,
    height: u32,
    regions: Reorientation,
) -> Option<Vec<u8>> {
    // 有的写入方在 XMP 包后补 NUL，PNG 的 iTXt 文本里不允许出现
    let mut xml = std::str::from_utf8(raw)
        .ok()?
        .trim_end_matches('\0')
        .to_owned();
    for (name, value) in [
        ("tiff:Orientation", 1),
        ("tiff:ImageWidth", width),
        ("tiff:ImageLength", height),
        ("exif:PixelXDimension", width),
        ("exif:PixelYDimension", height),
    ] {
        set_values(&mut xml, name, &value.to_string());
    }
    // MWG 规范要求区域坐标相对于应用 EXIF 方向之前的图像，输出已经转正，区域也得跟着转
    if regions != Reorientation::default() {
        let stored = if regions.transpose {
            (height, width)
        } else {
            (width, height)
        };
        for range in element_ranges(&xml, "mwg-rs:Area").into_iter().rev() {
            if let Some(area) = reorient_area(&xml[range.clone()], regions, stored) {
                xml.replace_range(range, &area);
            }
        }
    }
    for range in element_ranges(&xml, "mwg-rs:AppliedToDimensions")
        .into_iter()
        .rev()
    {
        let mut dims = xml[range.clone()].to_owned();
        let pixel = value(&dims, "stDim:unit").is_none_or(|unit| unit == "pixel");
        // 没转像素，记录的尺寸却横竖颠倒：区域是源文件里没随旋转更新的旧坐标，无从得知当初怎么转的，
        // 尺寸原样保留，不掩盖这个不一致
        let stale = regions == Reorientation::default()
            && value(&dims, "stDim:w")
                .zip(value(&dims, "stDim:h"))
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                .is_some_and(|(w, h)| (w > h && width < height) || (w < h && width > height));
        if pixel && !stale {
            set_values(&mut dims, "stDim:w", &width.to_string());
            set_values(&mut dims, "stDim:h", &height.to_string());
            xml.replace_range(range, &dims);
        }
    }
    Some(xml.into_bytes())
}

/// 区域的中心点和宽高按 `turn` 换算；圆的直径相对于短边，转了也不变。看不懂的区域返回 None，原样保留。
fn reorient_area(
    area: &str,
    turn: Reorientation,
    (stored_w, stored_h): (u32, u32),
) -> Option<String> {
    // 规范只定义了 normalized；有的软件写 pixel，按存储尺寸换算
    let (mut extent_x, mut extent_y) = match value(area, "stArea:unit") {
        None | Some("normalized") => (1.0, 1.0),
        Some("pixel") => (f64::from(stored_w), f64::from(stored_h)),
        Some(_) => return None,
    };
    let mut x = value(area, "stArea:x")?.to_owned();
    let mut y = value(area, "stArea:y")?.to_owned();
    let mut w = value(area, "stArea:w").map(str::to_owned);
    let mut h = value(area, "stArea:h").map(str::to_owned);
    if turn.transpose {
        std::mem::swap(&mut x, &mut y);
        std::mem::swap(&mut w, &mut h);
        std::mem::swap(&mut extent_x, &mut extent_y);
    }
    let mirror = |v: &str, extent: f64| -> Option<String> {
        let mirrored = extent - v.parse::<f64>().ok()?;
        // 6 位小数对 normalized 坐标已远小于一个像素
        let text = format!("{mirrored:.6}");
        Some(text.trim_end_matches('0').trim_end_matches('.').to_owned())
    };
    if turn.flip_x {
        x = mirror(&x, extent_x)?;
    }
    if turn.flip_y {
        y = mirror(&y, extent_y)?;
    }
    let mut area = area.to_owned();
    set_values(&mut area, "stArea:x", &x);
    set_values(&mut area, "stArea:y", &y);
    if let Some(w) = w {
        set_values(&mut area, "stArea:w", &w);
    }
    if let Some(h) = h {
        set_values(&mut area, "stArea:h", &h);
    }
    Some(area)
}

/// 每个 `<name ...>` 元素的范围：自闭合的到 `/>`，否则到 `</name>`。
fn element_ranges(xml: &str, name: &str) -> Vec<Range<usize>> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(found) = xml[from..].find(&open) {
        let start = from + found;
        from = start + open.len();
        // 名字后面必须是空白、`>` 或 `/`，免得匹配到更长的名字
        if !xml[from..].starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let Some(tag_end) = xml[from..].find('>').map(|i| from + i + 1) else {
            break;
        };
        let end = if xml[..tag_end].ends_with("/>") {
            tag_end
        } else {
            let Some(i) = xml[tag_end..].find(&close) else {
                break;
            };
            tag_end + i + close.len()
        };
        ranges.push(start..end);
        from = end;
    }
    ranges
}

/// 简单属性 `name` 在 `from` 之后第一处值的范围，`name="v"`、`name='v'`、`<name>v</name>` 三种写法都认。
/// 名字紧跟在名字字符后面的不算，免得 `tiff:Orientation` 匹配到别的前缀里。
fn value_range(xml: &str, name: &str, from: usize) -> Option<Range<usize>> {
    let forms = [
        (format!("{name}=\""), '"'),
        (format!("{name}='"), '\''),
        (format!("<{name}>"), '<'),
    ];
    forms
        .iter()
        .filter_map(|(open, close)| {
            let mut at = from;
            loop {
                let start = at + xml[at..].find(open.as_str())?;
                let value = start + open.len();
                at = value;
                let prev = xml[..start].chars().next_back();
                if prev.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')) {
                    continue;
                }
                return Some(value..value + xml[value..].find(*close)?);
            }
        })
        .min_by_key(|range| range.start)
}

fn value<'x>(xml: &'x str, name: &str) -> Option<&'x str> {
    value_range(xml, name, 0).map(|range| xml[range].trim())
}

/// 把 `name` 的每处值都换成 `value`
fn set_values(xml: &mut String, name: &str, value: &str) {
    let mut from = 0;
    while let Some(range) = value_range(xml, name, from) {
        from = range.start + value.len();
        xml.replace_range(range, value);
    }
}

const IMAGE_WIDTH: u16 = 0x0100;
const IMAGE_LENGTH: u16 = 0x0101;
const EXIF_IFD: u16 = 0x8769;
const PIXEL_X_DIMENSION: u16 = 0xA002;
const PIXEL_Y_DIMENSION: u16 = 0xA003;

/// 改写 IFD0 的 ImageWidth/ImageLength 和 Exif IFD 的 PixelXDimension/PixelYDimension。
/// 只改已有的条目，原地改写，不增删条目，偏移都不变。
fn set_exif_dimensions(exif: &mut [u8], width: u32, height: u32) {
    let big_endian = exif.starts_with(b"MM");
    let mut tiff = Tiff {
        data: exif,
        big_endian,
    };
    let Some(ifd0) = tiff.u32(4) else { return };
    let mut exif_ifd = None;
    for entry in tiff.entries(ifd0 as usize) {
        match tiff.u16(entry) {
            Some(IMAGE_WIDTH) => tiff.set_dimension(entry, width),
            Some(IMAGE_LENGTH) => tiff.set_dimension(entry, height),
            Some(EXIF_IFD) => exif_ifd = tiff.u32(entry + 8),
            _ => {}
        }
    }
    let Some(exif_ifd) = exif_ifd else { return };
    for entry in tiff.entries(exif_ifd as usize) {
        match tiff.u16(entry) {
            Some(PIXEL_X_DIMENSION) => tiff.set_dimension(entry, width),
            Some(PIXEL_Y_DIMENSION) => tiff.set_dimension(entry, height),
            _ => {}
        }
    }
}

/// 裸 TIFF 数据的读写，越界读当作没有。
struct Tiff<'a> {
    data: &'a mut [u8],
    big_endian: bool,
}

impl Tiff<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let bytes = self.data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let bytes = self.data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    fn put_u16(&mut self, at: usize, value: u16) {
        let bytes = if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        self.data[at..at + 2].copy_from_slice(&bytes);
    }

    fn put_u32(&mut self, at: usize, value: u32) {
        let bytes = if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        self.data[at..at + 4].copy_from_slice(&bytes);
    }

    /// IFD 里各条目（每条 12 字节）的偏移；条目表越界时当作空表
    fn entries(&self, ifd: usize) -> StepBy<Range<usize>> {
        let start = ifd.saturating_add(2);
        let count = self.u16(ifd).map_or(0, usize::from);
        let end = start.saturating_add(count * 12);
        let end = if end <= self.data.len() { end } else { start };
        (start..end).step_by(12)
    }

    /// 尺寸标签是 SHORT 或 LONG，单个值内联在条目最后 4 字节；SHORT 放不下时改成 LONG
    fn set_dimension(&mut self, entry: usize, value: u32) {
        const SHORT: u16 = 3;
        const LONG: u16 = 4;
        if self.u32(entry + 4) != Some(1) {
            return;
        }
        match (self.u16(entry + 2), u16::try_from(value)) {
            (Some(SHORT), Ok(short)) => {
                self.put_u16(entry + 8, short);
                self.put_u16(entry + 10, 0);
            }
            (Some(SHORT | LONG), _) => {
                self.put_u16(entry + 2, LONG);
                self.put_u32(entry + 8, value);
            }
            _ => {}
        }
    }
}

/// 只含 Orientation=6（顺时针转 90°）的小端 TIFF
#[cfg(test)]
pub const ROTATED_TIFF: &[u8] = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";

/// 只含 tiff:Orientation=6 的 XMP
#[cfg(test)]
pub const ROTATED_XMP: &[u8] = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:tiff="http://ns.adobe.com/tiff/1.0/" tiff:Orientation="6"/></rdf:RDF></x:xmpmeta>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_for_output_resets_orientation_and_strips_prefix() {
        let prefixed = [&b"Exif\0\0"[..], ROTATED_TIFF].concat();
        for raw in [ROTATED_TIFF, &prefixed[..]] {
            let out = exif_for_output(raw, 4, 8).unwrap();
            assert!(out.starts_with(b"II*\0"));
            assert_eq!(out.len(), ROTATED_TIFF.len());
            assert_eq!(
                Orientation::from_exif_chunk(&out),
                Some(Orientation::NoTransforms)
            );
        }
        assert_eq!(exif_for_output(b"not exif", 4, 8), None);
    }

    /// IFD0：ImageWidth(SHORT)、ImageLength(LONG)、Orientation=6、Exif IFD 指针；
    /// Exif IFD：PixelXDimension、PixelYDimension（都是 SHORT）。返回数据和各尺寸条目的偏移。
    fn tiff_with_dimensions(big_endian: bool) -> (Vec<u8>, [usize; 4]) {
        let mut t = Vec::new();
        let u16b = |v: u16| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let entry = |t: &mut Vec<u8>, tag: u16, kind: u16, value: u32| {
            t.extend(u16b(tag));
            t.extend(u16b(kind));
            t.extend(u32b(1));
            if kind == 3 {
                t.extend(u16b(value as u16));
                t.extend([0, 0]);
            } else {
                t.extend(u32b(value));
            }
        };
        t.extend(if big_endian { *b"MM\0*" } else { *b"II*\0" });
        t.extend(u32b(8));
        // IFD0 在 8，4 个条目，长 2 + 4*12 + 4 = 54；Exif IFD 在 62
        t.extend(u16b(4));
        entry(&mut t, IMAGE_WIDTH, 3, 5712);
        entry(&mut t, IMAGE_LENGTH, 4, 4284);
        entry(&mut t, 0x0112, 3, 6);
        entry(&mut t, EXIF_IFD, 4, 62);
        t.extend(u32b(0));
        t.extend(u16b(2));
        entry(&mut t, PIXEL_X_DIMENSION, 3, 5712);
        entry(&mut t, PIXEL_Y_DIMENSION, 3, 4284);
        t.extend(u32b(0));
        (t, [10, 22, 64, 76])
    }

    #[test]
    fn exif_for_output_sets_dimensions() {
        for big_endian in [false, true] {
            let (raw, [width, length, pixel_x, pixel_y]) = tiff_with_dimensions(big_endian);
            // 高度超出 SHORT，条目要改成 LONG
            let mut out = exif_for_output(&raw, 4284, 70000).unwrap();
            assert_eq!(out.len(), raw.len());
            assert_eq!(
                Orientation::from_exif_chunk(&out),
                Some(Orientation::NoTransforms)
            );
            let tiff = Tiff {
                data: &mut out,
                big_endian,
            };
            let value = |entry: usize| match tiff.u16(entry + 2) {
                Some(3) => (3, u32::from(tiff.u16(entry + 8).unwrap())),
                kind => (kind.unwrap(), tiff.u32(entry + 8).unwrap()),
            };
            assert_eq!(value(width), (3, 4284));
            assert_eq!(value(length), (4, 70000));
            assert_eq!(value(pixel_x), (3, 4284));
            assert_eq!(value(pixel_y), (4, 70000));
        }
    }

    #[test]
    fn exif_for_output_ignores_truncated_ifd() {
        let (raw, _) = tiff_with_dimensions(false);
        let out = exif_for_output(&raw[..40], 1, 1).unwrap();
        assert_eq!(out, raw[..40]);
    }

    #[test]
    fn xmp_for_output_rewrites_orientation_and_dimensions() {
        let xmp = concat!(
            r#"<rdf:Description tiff:Orientation="6" exif:PixelXDimension='5712'"#,
            r#" xtiff:Orientation="6">"#,
            "<exif:PixelYDimension>4284</exif:PixelYDimension>",
            "<stDim:w>5712</stDim:w></rdf:Description>\0\0",
        );
        let out = xmp_for_output(xmp.as_bytes(), 4284, 5712, Reorientation::from_exif(6)).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                r#"<rdf:Description tiff:Orientation="1" exif:PixelXDimension='4284'"#,
                r#" xtiff:Orientation="6">"#,
                "<exif:PixelYDimension>5712</exif:PixelYDimension>",
                "<stDim:w>5712</stDim:w></rdf:Description>",
            )
        );
        assert_eq!(
            xmp_for_output(b"\xff\xfe<x/>", 1, 1, Reorientation::default()),
            None
        );
    }

    #[test]
    fn reorientation_composes() {
        let exif = Reorientation::from_exif;
        assert_eq!(exif(1), Reorientation::default());
        assert_eq!(exif(6).then(exif(6)), exif(3));
        assert_eq!(exif(6).then(exif(8)), exif(1));
        // 先左右翻转再顺时针转 90° 等于 transverse
        assert_eq!(exif(2).then(exif(6)), exif(7));
        assert_eq!(exif(4).then(exif(6)), exif(5));
    }

    /// Reorientation 对点的映射要和 image 实际转像素的结果一致
    #[test]
    fn reorientation_matches_image_apply_orientation() {
        use image::{DynamicImage, Rgba, RgbaImage};
        let (w, h) = (3u32, 2u32);
        let src = RgbaImage::from_fn(w, h, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        for value in 1..=8 {
            let mut img = DynamicImage::ImageRgba8(src.clone());
            img.apply_orientation(Orientation::from_exif(value).unwrap());
            let out = img.into_rgba8();
            let t = Reorientation::from_exif(value);
            for (sx, sy) in [(0, 0), (2, 0), (0, 1)] {
                let (mut x, mut y, mut ex, mut ey) = (sx, sy, w - 1, h - 1);
                if t.transpose {
                    (x, y, ex, ey) = (y, x, ey, ex);
                }
                if t.flip_x {
                    x = ex - x;
                }
                if t.flip_y {
                    y = ey - y;
                }
                assert_eq!(
                    out.get_pixel(x, y).0[..2],
                    [sx as u8, sy as u8],
                    "orientation {value}"
                );
            }
        }
    }

    #[test]
    fn xmp_regions_follow_rotation() {
        // iPhone 的写法：元素形式，坐标和 AppliedToDimensions 都按转正前的横图
        let apple = concat!(
            r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Seq>"#,
            r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Area rdf:parseType="Resource">"#,
            "<stArea:y>0.519</stArea:y><stArea:w>0.118</stArea:w>",
            "<stArea:x>0.6126</stArea:x><stArea:h>0.158</stArea:h>",
            "<stArea:unit>normalized</stArea:unit></mwg-rs:Area></rdf:li></rdf:Seq>",
            r#"</mwg-rs:RegionList><mwg-rs:AppliedToDimensions rdf:parseType="Resource">"#,
            "<stDim:h>4284</stDim:h><stDim:w>5712</stDim:w><stDim:unit>pixel</stDim:unit>",
            "</mwg-rs:AppliedToDimensions></mwg-rs:Regions>",
        );
        let out =
            xmp_for_output(apple.as_bytes(), 4284, 5712, Reorientation::from_exif(6)).unwrap();
        // 顺时针转 90°：(x, y) → (1 - y, x)，宽高对调
        assert_eq!(
            String::from_utf8(out).unwrap(),
            apple
                .replace("<stArea:y>0.519<", "<stArea:y>0.6126<")
                .replace("<stArea:w>0.118<", "<stArea:w>0.158<")
                .replace("<stArea:x>0.6126<", "<stArea:x>0.481<")
                .replace("<stArea:h>0.158<", "<stArea:h>0.118<")
                .replace("<stDim:h>4284<", "<stDim:h>5712<")
                .replace("<stDim:w>5712<", "<stDim:w>4284<")
        );

        // MWG 规范示例的属性写法，另含一个圆形区域
        let spec = concat!(
            r#"<mwg-rs:AppliedToDimensions stDim:w="4288" stDim:h="2848" stDim:unit="pixel"/>"#,
            r#"<mwg-rs:Area stArea:x="0.2" stArea:y="0.25" stArea:w="0.06" stArea:h="0.09" stArea:unit="normalized"/>"#,
            r#"<mwg-rs:Area stArea:x="0.3" stArea:y="0.4" stArea:d="0.1" stArea:unit="normalized"/>"#,
        );
        // 逆时针转 90°：(x, y) → (y, 1 - x)
        let out = xmp_for_output(spec.as_bytes(), 2848, 4288, Reorientation::from_exif(8)).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                r#"<mwg-rs:AppliedToDimensions stDim:w="2848" stDim:h="4288" stDim:unit="pixel"/>"#,
                r#"<mwg-rs:Area stArea:x="0.25" stArea:y="0.8" stArea:w="0.09" stArea:h="0.06" stArea:unit="normalized"/>"#,
                r#"<mwg-rs:Area stArea:x="0.4" stArea:y="0.7" stArea:d="0.1" stArea:unit="normalized"/>"#,
            )
        );

        // 没转像素时区域不动；尺寸只是缩放过就更新
        let out = xmp_for_output(spec.as_bytes(), 4288, 2848, Reorientation::default()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), spec);
        let out = xmp_for_output(spec.as_bytes(), 2144, 1424, Reorientation::default()).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            spec.replace(
                r#"stDim:w="4288" stDim:h="2848""#,
                r#"stDim:w="2144" stDim:h="1424""#
            )
        );
    }

    #[test]
    fn xmp_keeps_stale_regions() {
        // 源文件已转正但区域没更新（尺寸横竖颠倒），sqz 也没转像素：区域和尺寸都原样保留
        let stale = concat!(
            r#"<mwg-rs:AppliedToDimensions stDim:w="5712" stDim:h="4284" stDim:unit="pixel"/>"#,
            r#"<mwg-rs:Area stArea:x="0.6" stArea:y="0.5" stArea:w="0.1" stArea:h="0.2" stArea:unit="normalized"/>"#,
        );
        let out = xmp_for_output(stale.as_bytes(), 4284, 5712, Reorientation::default()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), stale);
    }
}
