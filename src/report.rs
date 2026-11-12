use std::path::Path;

use crate::encode::OutputFormat;

pub const PNG_JPEG_HINT: &str = "照片类图片可改用 -f jpg";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    KeptOriginal,
    BestEffort,
    SizeUnreachable,
    Error,
}

#[derive(Debug)]
pub struct Report {
    pub input: String,
    pub status: Status,
    pub output: Option<String>,
    pub format: Option<OutputFormat>,
    pub input_bytes: Option<u64>,
    pub output_bytes: Option<u64>,
    pub quality: Option<u8>,
    pub dither: Option<f32>,
    pub tries: u32,
    pub error: Option<String>,
}

impl Report {
    pub fn new(input: &Path) -> Self {
        Self {
            input: input.display().to_string(),
            status: Status::Error,
            output: None,
            format: None,
            input_bytes: None,
            output_bytes: None,
            quality: None,
            dither: None,
            tries: 0,
            error: None,
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

pub fn print_reports(reports: &[Report]) {
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
