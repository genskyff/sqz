use std::path::Path;

use serde::Serialize;

use crate::encode::OutputFormat;

pub const PNG_JPEG_HINT: &str = "照片类图片可改用 -f jpg";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    KeptOriginal,
    BestEffort,
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
    /// 转正后、缩小前的尺寸
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_height: Option<u32>,
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
            input_width: None,
            input_height: None,
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn print_json(reports: &[Report]) {
    println!(
        "{}",
        serde_json::to_string_pretty(reports).expect("报告可序列化")
    );
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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
                if let Some(from) = r.input_width.zip(r.input_height)
                    && let Some(to) = r.width.zip(r.height)
                    && from != to
                {
                    line.push_str(&format!("  {}×{} → {}×{}", from.0, from.1, to.0, to.1));
                }
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
                    Status::KeptOriginal => line.push_str("  未变小，保留原图"),
                    Status::BestEffort if r.format == Some(OutputFormat::Png) => {
                        line.push_str(&format!("  未达上限，已写出最小结果；{PNG_JPEG_HINT}"))
                    }
                    Status::BestEffort => line.push_str("  未达上限，已写出最小结果"),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_omits_fields_without_a_value() {
        let mut report = Report::new(Path::new("a.heic"));
        let value = |r: &Report| serde_json::to_value(r).unwrap();
        assert_eq!(
            value(&report),
            json!({ "input": "a.heic", "status": "error", "tries": 0, "elapsed_ms": 0 })
        );

        report.status = Status::KeptOriginal;
        report.format = Some(OutputFormat::Webp);
        (report.width, report.height) = (Some(1200), Some(1600));
        (report.input_width, report.input_height) = (Some(3072), Some(4096));
        report.quality = Some(60);
        assert_eq!(
            value(&report),
            json!({
                "input": "a.heic",
                "status": "kept_original",
                "format": "webp",
                "width": 1200,
                "height": 1600,
                "input_width": 3072,
                "input_height": 4096,
                "quality": 60,
                "tries": 0,
                "elapsed_ms": 0,
            })
        );
    }
}
