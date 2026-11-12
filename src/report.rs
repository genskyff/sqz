use std::path::Path;

use serde::Serialize;

use crate::encode::OutputFormat;

pub const PNG_JPEG_HINT: &str = "PNG 降质只会减少颜色，照片类图片可改用 -f jpg";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// 同格式压缩后不比原图小，输出为原图副本
    KeptOriginal,
    /// 达不到 --max-size，按 --best-effort 写出了最小结果
    BestEffort,
    /// 达不到 --max-size，未写出文件
    SizeUnreachable,
    Error,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub input: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dither: Option<f32>,
    pub tries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

impl Report {
    pub fn new(input: &Path) -> Self {
        Self {
            input: input.display().to_string(),
            status: Status::Error,
            output: None,
            format: None,
            width: None,
            height: None,
            input_bytes: None,
            output_bytes: None,
            quality: None,
            dither: None,
            tries: 0,
            error: None,
            elapsed_ms: 0,
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self.status, Status::Error | Status::SizeUnreachable)
    }
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.1} KB", bytes as f64 / 1e3),
        _ => format!("{:.2} MB", bytes as f64 / 1e6),
    }
}

pub fn print_human(reports: &[Report]) {
    for r in reports {
        let message = r.error.as_deref().unwrap_or("未知错误");
        match r.status {
            Status::Error => eprintln!("{}：错误：{message}", r.input),
            Status::SizeUnreachable => eprintln!("{}：{message}", r.input),
            Status::Ok | Status::KeptOriginal | Status::BestEffort => {
                let (input_bytes, output_bytes) =
                    (r.input_bytes.unwrap_or(0), r.output_bytes.unwrap_or(0));
                let change = (output_bytes as f64 / input_bytes.max(1) as f64 - 1.0) * 100.0;
                let mut line = format!(
                    "{} → {}  {} → {}  {change:+.1}%",
                    r.input,
                    r.output.as_deref().unwrap_or("?"),
                    human_size(input_bytes),
                    human_size(output_bytes),
                );
                if let Some(q) = r.quality {
                    line.push_str(&format!("  q={q}"));
                }
                if let Some(d) = r.dither.filter(|&d| d < 1.0) {
                    line.push_str(&format!("  dither={d}"));
                }
                if r.tries > 1 {
                    line.push_str(&format!("  试了 {} 次", r.tries));
                }
                match r.status {
                    Status::KeptOriginal => line.push_str("  （压缩后不更小，保留原图）"),
                    Status::BestEffort if r.format == Some(OutputFormat::Png) => line.push_str(
                        &format!("  （未达到大小上限，已写出最小结果；{PNG_JPEG_HINT}）"),
                    ),
                    Status::BestEffort => line.push_str("  （未达到大小上限，已写出最小结果）"),
                    _ => {}
                }
                println!("{line}");
            }
        }
    }

    if reports.len() > 1 {
        let done: Vec<&Report> = reports.iter().filter(|r| !r.failed()).collect();
        let total_in: u64 = done.iter().filter_map(|r| r.input_bytes).sum();
        let total_out: u64 = done.iter().filter_map(|r| r.output_bytes).sum();
        println!(
            "共 {} 个：成功 {}，失败 {}；{} → {}",
            reports.len(),
            done.len(),
            reports.len() - done.len(),
            human_size(total_in),
            human_size(total_out)
        );
    }
}
