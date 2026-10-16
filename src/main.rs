mod cli;
mod decode;
mod jpeg;
mod png;
mod search;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;

use cli::{Cli, FormatArg};
use decode::InputFormat;
use jpeg::JpegJob;
use png::{Effort, PngJob};
use search::{Fit, fit_size};

const DEFAULT_QUALITY: u8 = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Jpeg,
    Png,
}

impl OutputFormat {
    fn name(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "jpeg",
            OutputFormat::Png => "png",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Png => "png",
        }
    }

    fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(OutputFormat::Jpeg),
            "png" => Some(OutputFormat::Png),
            _ => None,
        }
    }

    fn same_as(self, input: InputFormat) -> bool {
        matches!(
            (self, input),
            (OutputFormat::Jpeg, InputFormat::Jpeg) | (OutputFormat::Png, InputFormat::Png)
        )
    }

    /// 只给 --max-size 时的质量上限：JPEG 超过 95 体积暴涨而肉眼难辨；PNG 从无损开始试
    fn size_mode_ceiling(self) -> u8 {
        match self {
            OutputFormat::Jpeg => 95,
            OutputFormat::Png => 100,
        }
    }
}

enum Encoder<'a> {
    Jpeg(JpegJob<'a>),
    Png(PngJob<'a>),
}

impl Encoder<'_> {
    fn encode(&self, quality: u8, effort: Effort) -> Result<Vec<u8>> {
        match self {
            Encoder::Jpeg(job) => job.encode(quality),
            Encoder::Png(job) => job.encode(quality, effort),
        }
    }

    /// PNG 无损搜索时用的是较快档位，定稿再用最高档重压并取更小者，因此不会超出上限。
    fn refine(&self, quality: u8, searched: Vec<u8>) -> Result<Vec<u8>> {
        match self {
            Encoder::Png(job) if quality >= 100 => {
                let refined = job.encode(quality, Effort::Final)?;
                Ok(if refined.len() < searched.len() {
                    refined
                } else {
                    searched
                })
            }
            _ => Ok(searched),
        }
    }

    /// PNG 有损输出所用的抖动强度。
    fn dither(&self) -> Option<f32> {
        match self {
            Encoder::Jpeg(_) => None,
            Encoder::Png(job) => Some(job.dither()),
        }
    }

    /// 关掉 PNG 抖动以换取更小体积；返回是否真的有所改变。
    fn drop_dither(&mut self) -> bool {
        match self {
            Encoder::Png(job) if job.dither() > 0.0 => {
                job.disable_dither();
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
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
struct Report {
    input: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dither: Option<f32>,
    tries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    elapsed_ms: u64,
}

impl Report {
    fn new(input: &Path) -> Self {
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

    fn failed(&self) -> bool {
        matches!(self.status, Status::Error | Status::SizeUnreachable)
    }
}

struct Plan {
    input_format: InputFormat,
    format: OutputFormat,
    output: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.output.is_some() && cli.inputs.len() > 1 {
        eprintln!("错误：-o 只能用于单个输入，多个输入请用 -d 指定输出目录");
        return ExitCode::from(2);
    }
    if let Some(threads) = cli.jobs
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
    {
        eprintln!("错误：无法创建线程池：{e}");
        return ExitCode::from(2);
    }

    let plans: Vec<(&PathBuf, Result<Plan>)> = cli
        .inputs
        .iter()
        .map(|input| (input, plan(&cli, input)))
        .collect();

    let mut claimed: HashMap<String, &Path> = HashMap::new();
    for (input, plan) in &plans {
        if let Ok(plan) = plan
            && let Some(other) = claimed.insert(path_key(&plan.output), input)
        {
            eprintln!(
                "错误：{} 和 {} 会写到同一个文件 {}，请用 --suffix 或分开处理",
                other.display(),
                input.display(),
                plan.output.display()
            );
            return ExitCode::from(2);
        }
    }

    let reports: Vec<Report> = plans
        .par_iter()
        .map(|(input, plan)| {
            let start = Instant::now();
            let mut report = Report::new(input);
            let outcome = match plan {
                Ok(plan) => process(&cli, input, plan, &mut report),
                Err(e) => Err(anyhow!("{e:#}")),
            };
            if let Err(e) = outcome {
                report.error = Some(format!("{e:#}"));
            }
            report.elapsed_ms = start.elapsed().as_millis() as u64;
            report
        })
        .collect();

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&reports).expect("报告可序列化")
        );
    } else {
        print_human(&reports);
    }

    if reports.iter().any(Report::failed) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn plan(cli: &Cli, input: &Path) -> Result<Plan> {
    let mut head = Vec::with_capacity(512);
    File::open(input)
        .context("无法打开文件")?
        .take(512)
        .read_to_end(&mut head)
        .context("读取失败")?;
    let input_format =
        decode::sniff(&head).context("不支持的输入格式（仅支持 PNG、JPEG、HEIC）")?;

    let format = match cli.format {
        FormatArg::Jpeg => OutputFormat::Jpeg,
        FormatArg::Png => OutputFormat::Png,
        FormatArg::Auto => cli
            .output
            .as_deref()
            .and_then(OutputFormat::from_path)
            .unwrap_or(match input_format {
                InputFormat::Png => OutputFormat::Png,
                InputFormat::Jpeg | InputFormat::Heic => OutputFormat::Jpeg,
            }),
    };

    let output = output_path(cli, input, format);
    if !cli.overwrite && output.exists() {
        if same_file(&output, input) {
            bail!("输出路径就是输入文件，加 --overwrite 才能原地覆盖");
        }
        bail!(
            "输出文件已存在：{}（加 --overwrite 覆盖）",
            output.display()
        );
    }
    Ok(Plan {
        input_format,
        format,
        output,
    })
}

fn output_path(cli: &Cli, input: &Path, format: OutputFormat) -> PathBuf {
    if let Some(output) = &cli.output {
        return output.clone();
    }
    let dir = cli
        .out_dir
        .as_deref()
        .or_else(|| input.parent())
        .unwrap_or(Path::new(""));
    let suffix = cli
        .suffix
        .as_deref()
        .unwrap_or(if cli.out_dir.is_some() { "" } else { ".min" });
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    // 原扩展名与输出格式一致时沿用原写法（如 .jpeg、.JPG），否则用标准扩展名
    let ext = match input.extension() {
        Some(ext) if OutputFormat::from_path(input) == Some(format) => ext.to_string_lossy(),
        _ => format.extension().into(),
    };
    dir.join(format!("{stem}{suffix}.{ext}"))
}

fn process(cli: &Cli, input: &Path, plan: &Plan, report: &mut Report) -> Result<()> {
    let original = fs::read(input).context("读取失败")?;
    report.input_bytes = Some(original.len() as u64);
    let decoded = decode::decode(&original)?;
    let (width, height) = (decoded.width, decoded.height);
    report.width = Some(width);
    report.height = Some(height);

    let icc = if cli.strip_icc { None } else { decoded.icc };
    let mut encoder = match plan.format {
        OutputFormat::Jpeg => Encoder::Jpeg(JpegJob::new(
            width,
            height,
            decoded.rgba,
            cli.chroma,
            cli.background,
            icc.as_deref(),
        )),
        OutputFormat::Png => Encoder::Png(PngJob::new(
            width,
            height,
            decoded.rgba,
            cli.colors,
            cli.dither.unwrap_or(1.0),
            icc.as_deref(),
        )),
    };

    let (mut data, mut status) = match cli.max_size {
        None => {
            let quality = cli.quality.unwrap_or(DEFAULT_QUALITY);
            report.quality = Some(quality);
            report.tries = 1;
            (encoder.encode(quality, Effort::Final)?, Status::Ok)
        }
        Some(cap) => {
            let ceiling = cli.quality.unwrap_or(plan.format.size_mode_ceiling());
            let mut result = fit_size(cli.min_quality, ceiling, cap, |q| {
                encoder.encode(q, Effort::Search)
            })?;
            // 抖动噪点让 PNG 很难再变小：未指定 --dither 时，达不到上限就关掉抖动再搜一次
            if matches!(result.fit, Fit::Unreachable { .. })
                && cli.dither.is_none()
                && encoder.drop_dither()
            {
                let retry = fit_size(cli.min_quality, ceiling.min(99), cap, |q| {
                    encoder.encode(q, Effort::Search)
                })?;
                result.tries += retry.tries;
                result.fit = retry.fit;
            }
            report.tries = result.tries;
            let (quality, data, status) = match result.fit {
                Fit::Found { quality, data } => (quality, data, Status::Ok),
                Fit::Unreachable { quality, data } if cli.best_effort => {
                    (quality, data, Status::BestEffort)
                }
                Fit::Unreachable { quality, data } => {
                    report.status = Status::SizeUnreachable;
                    report.quality = Some(quality);
                    report.dither = encoder.dither().filter(|_| quality < 100);
                    report.output_bytes = Some(data.len() as u64);
                    bail!(
                        "质量降到 {quality} 仍有 {}，超过上限 {}（可降低 --min-quality，或加 --best-effort 写出最小结果）",
                        human_size(data.len() as u64),
                        human_size(cap)
                    );
                }
            };
            report.quality = Some(quality);
            (encoder.refine(quality, data)?, status)
        }
    };
    if report.quality.is_some_and(|q| q < 100) {
        report.dither = encoder.dither();
    }

    let kept_original =
        plan.format.same_as(plan.input_format) && !cli.allow_larger && data.len() >= original.len();
    if kept_original {
        data = original;
        report.quality = None;
        report.dither = None;
        if status == Status::Ok {
            status = Status::KeptOriginal;
        }
    }

    if !(kept_original && same_file(&plan.output, input)) {
        write_atomic(&plan.output, &data)?;
    }
    report.status = status;
    report.format = Some(plan.format.name());
    report.output = Some(plan.output.display().to_string());
    report.output_bytes = Some(data.len() as u64);
    Ok(())
}

/// 先写临时文件再改名，中途失败不会留下半截输出。
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".sqz-tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, data).with_context(|| format!("写入 {} 失败", tmp.display()))?;
    fs::rename(&tmp, path)
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .with_context(|| format!("写入 {} 失败", path.display()))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 用于判断两个输出路径是否指向同一文件（文件可能还不存在）。
fn path_key(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let key = abs.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.1} KB", bytes as f64 / 1e3),
        _ => format!("{:.2} MB", bytes as f64 / 1e6),
    }
}

fn print_human(reports: &[Report]) {
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
