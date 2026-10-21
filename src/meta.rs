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

/// XMP 里的方向和尺寸同 EXIF 一样改成输出后的值；不是 UTF-8 的 XMP 无法改写，丢弃。
pub fn xmp_for_output(raw: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
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
    Some(xml.into_bytes())
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
        let out = xmp_for_output(xmp.as_bytes(), 4284, 5712).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                r#"<rdf:Description tiff:Orientation="1" exif:PixelXDimension='4284'"#,
                r#" xtiff:Orientation="6">"#,
                "<exif:PixelYDimension>5712</exif:PixelYDimension>",
                "<stDim:w>5712</stDim:w></rdf:Description>",
            )
        );
        assert_eq!(xmp_for_output(b"\xff\xfe<x/>", 1, 1), None);
    }
}
