use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// 压缩 PNG / JPEG / HEIC / WebP 图片。
///
/// 默认按质量压缩（-q，默认 75）；给出 --max-size 时，在不超过该大小的前提下反推最高质量。
///
/// 输出格式默认：PNG→PNG，JPEG→JPEG，HEIC→JPEG，WebP→WebP。
#[derive(Debug, Parser)]
#[command(name = "sqz", version)]
pub struct Cli {
    /// 输入文件，可传多个，支持 * 和 ? 通配符
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,

    /// 输出文件（仅限单个输入）；未指定 -f 时按扩展名推断格式
    #[arg(short, long, conflicts_with = "out_dir")]
    pub output: Option<PathBuf>,

    /// 输出目录；不指定 -o/-d 时输出到输入文件所在目录
    #[arg(short = 'd', long)]
    pub out_dir: Option<PathBuf>,

    /// 输出文件名后缀，默认：使用 -d 时为空，否则为 ".min"
    #[arg(long)]
    pub suffix: Option<String>,

    /// 输出格式
    #[arg(short, long, value_enum, ignore_case = true, default_value_t = FormatArg::Auto)]
    pub format: FormatArg,

    /// 只转换格式，不降质：PNG、WebP 无损，JPEG 用 q100。须用 -f 或 -o 的扩展名指定目标格式
    #[arg(long, conflicts_with_all = ["quality", "max_size", "min_quality", "best_effort", "colors", "dither"])]
    pub convert_only: bool,

    /// 质量 0-100（JPEG 刻度与 mozjpeg 相近；PNG 为 100 时无损；WebP 为有损）。与 --max-size 同用时作为质量上限，单用 --max-size 时上限为 JPEG/WebP 95、PNG 100
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub quality: Option<u8>,

    /// 输出大小上限，如 500KB、1.5MB、300KiB（KB=1000，KiB=1024）
    #[arg(short = 's', long, value_parser = parse_size)]
    pub max_size: Option<u64>,

    /// --max-size 搜索的质量下限，默认 JPEG/WebP 1、PNG 30。PNG 降质靠减少颜色，低于 30 时颜色会严重失真
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub min_quality: Option<u8>,

    /// 达不到 --max-size 时仍写出能做到的最小结果（默认不写并报错）
    #[arg(long)]
    pub best_effort: bool,

    /// JPEG 色度采样
    #[arg(long, value_enum, default_value_t = Chroma::C420)]
    pub chroma: Chroma,

    /// 透明图片转 JPEG 时铺的底色（十六进制 RGB）
    #[arg(long, default_value = "ffffff", value_parser = parse_color)]
    pub background: Rgb,

    /// PNG 有损压缩的最大颜色数 2-256
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(2..=256))]
    pub colors: u32,

    /// PNG 有损压缩的抖动强度 0-1，默认 1。未指定时，若开着抖动达不到 --max-size，会关掉抖动再搜一次
    #[arg(long, value_parser = parse_unit)]
    pub dither: Option<f32>,

    /// 不保留 ICC 色彩配置文件
    #[arg(long)]
    pub strip_icc: bool,

    /// 不保留 EXIF（默认保留，方向已应用到像素上，其中的方向重置为正常、像素尺寸改为输出尺寸）
    #[arg(long)]
    pub strip_exif: bool,

    /// 不保留 XMP（默认保留，方向和像素尺寸的处理同 EXIF）
    #[arg(long)]
    pub strip_xmp: bool,

    /// 同格式输出不比原文件小时仍写出结果（默认改为原样复制原文件）
    #[arg(long)]
    pub allow_larger: bool,

    /// 允许覆盖已存在的输出文件（包括输入文件本身）
    #[arg(long)]
    pub overwrite: bool,

    /// 并行线程数，默认为 CPU 核数
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// 以 JSON 输出结果（stdout），便于脚本和 agent 解析
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    Auto,
    #[value(alias = "jpg")]
    Jpeg,
    Png,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Chroma {
    /// 4:2:0，体积小，适合照片
    #[value(name = "420")]
    C420,
    /// 4:4:4，保留全部色度，适合文字、线条
    #[value(name = "444")]
    C444,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub [u8; 3]);

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().map_err(|_| format!("无效的大小：{s}"))?;
    let unit_bytes: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        other => {
            return Err(format!(
                "无法识别的单位 {other:?}，可用 B、KB、MB、KiB、MiB"
            ));
        }
    };
    let bytes = (value * unit_bytes).floor();
    if bytes < 1.0 {
        return Err("大小必须至少 1 字节".into());
    }
    Ok(bytes as u64)
}

fn parse_color(s: &str) -> Result<Rgb, String> {
    let hex = s.trim().trim_start_matches('#');
    let err = || format!("无效的颜色 {s:?}，应为 6 位十六进制，如 ffffff");
    if hex.len() != 6 {
        return Err(err());
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| err());
    Ok(Rgb([channel(0)?, channel(2)?, channel(4)?]))
}

fn parse_unit(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("无效的数值：{s}"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err("取值范围为 0-1".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("500KB"), Ok(500_000));
        assert_eq!(parse_size("1.5mb"), Ok(1_500_000));
        assert_eq!(parse_size("300KiB"), Ok(307_200));
        assert_eq!(parse_size("2048"), Ok(2048));
        assert_eq!(parse_size(" 2 M "), Ok(2_000_000));
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10GB").is_err());
        assert!(parse_size("0").is_err());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#FF8000"), Ok(Rgb([255, 128, 0])));
        assert!(parse_color("fff").is_err());
    }

    #[test]
    fn format_names() {
        let format =
            |name: &str| Cli::try_parse_from(["sqz", "a.png", "-f", name]).map(|c| c.format);
        for name in ["jpeg", "jpg", "JPG", "Jpeg"] {
            assert_eq!(format(name).unwrap(), FormatArg::Jpeg, "{name}");
        }
        assert_eq!(format("webp").unwrap(), FormatArg::Webp);
        assert!(format("gif").is_err());
    }

    #[test]
    fn convert_only_rejects_quality_options() {
        let parse = |extra: &[&str]| {
            Cli::try_parse_from(
                ["sqz", "a.png", "-f", "webp", "--convert-only"]
                    .iter()
                    .chain(extra),
            )
        };
        assert!(parse(&[]).is_ok());
        assert!(parse(&["--chroma", "444"]).is_ok());
        for extra in [
            &["-q", "80"][..],
            &["-s", "100KB"],
            &["--min-quality", "10"],
            &["--best-effort"],
            &["--colors", "64"],
            &["--dither", "0"],
        ] {
            assert!(parse(extra).is_err(), "{extra:?}");
        }
    }
}
