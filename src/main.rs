mod cli;
mod decode;
mod jpeg;
mod png;
mod search;
mod webp;

use std::collections::{HashMap, HashSet};
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
use search::{Fit, Order, climb, fit_size, retest_top};
use webp::WebpJob;

const DEFAULT_QUALITY: u8 = 75;

const PNG_JPEG_HINT: &str = "PNG 降质只会减少颜色，照片类图片可改用 -f jpg";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Jpeg,
    Png,
    Webp,
}

impl OutputFormat {
    fn name(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "jpeg",
            OutputFormat::Png => "png",
            OutputFormat::Webp => "webp",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Png => "png",
            OutputFormat::Webp => "webp",
        }
    }

    fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(OutputFormat::Jpeg),
            "png" => Some(OutputFormat::Png),
            "webp" => Some(OutputFormat::Webp),
            _ => None,
        }
    }

    fn same_as(self, input: InputFormat) -> bool {
        matches!(
            (self, input),
            (OutputFormat::Jpeg, InputFormat::Jpeg)
                | (OutputFormat::Png, InputFormat::Png)
                | (OutputFormat::Webp, InputFormat::Webp)
        )
    }

    /// 只给 --max-size 时的质量上限：有损格式超过 95 体积暴涨而肉眼难辨；PNG 从无损开始试
    fn size_mode_ceiling(self) -> u8 {
        match self {
            OutputFormat::Jpeg | OutputFormat::Webp => 95,
            OutputFormat::Png => 100,
        }
    }

    /// 未给 --min-quality 时的质量下限：PNG 降质靠减少颜色，低于 30 时只剩几种颜色
    fn size_mode_floor(self) -> u8 {
        match self {
            OutputFormat::Jpeg | OutputFormat::Webp => 1,
            OutputFormat::Png => 30,
        }
    }
}

enum Encoder<'a> {
    Jpeg(JpegJob<'a>),
    Png(PngJob<'a>),
    Webp(WebpJob<'a>),
}

impl Encoder<'_> {
    fn encode(&self, quality: u8, effort: Effort) -> Result<Vec<u8>> {
        match self {
            Encoder::Jpeg(job) => job.encode(quality),
            Encoder::Png(job) => job.encode(quality, effort),
            Encoder::Webp(job) => job.encode(quality),
        }
    }

    /// --convert-only 的编码：PNG、WebP 无损，JPEG 没有无损模式，用最高质量
    fn convert(&self) -> Result<Vec<u8>> {
        match self {
            Encoder::Jpeg(job) => job.encode(100),
            Encoder::Png(job) => job.encode(100, Effort::Final),
            Encoder::Webp(job) => job.lossless(),
        }
    }

    /// PNG 搜索时用的是快速档位，定稿再用最高档重压并取更小者，因此不会超出上限。
    fn refine(&self, quality: u8, searched: Vec<u8>) -> Result<Vec<u8>> {
        match self {
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

    fn dither(&self) -> Option<f32> {
        match self {
            Encoder::Jpeg(_) | Encoder::Webp(_) => None,
            Encoder::Png(job) => Some(job.dither()),
        }
    }

    /// 关掉 PNG 抖动；返回是否真的有所改变。
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
    let mut cli = Cli::parse();
    cli.inputs = expand_inputs(&cli.inputs);
    if cli.output.is_some() && cli.inputs.len() > 1 {
        eprintln!("错误：-o 只能用于单个输入，多个输入请用 -d 指定输出目录");
        return ExitCode::from(2);
    }
    if cli.convert_only && explicit_format(&cli).is_none() {
        eprintln!("错误：--convert-only 须用 -f 或 -o 的扩展名指定目标格式");
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

/// 参数含 `*` 或 `?` 且不是现有文件时按通配符展开（PowerShell 和 cmd 不替程序展开），
/// 结果去重；没有匹配的模式原样保留，由后续步骤报错。
fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let options = glob::MatchOptions {
        case_sensitive: !cfg!(windows),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut expanded = Vec::new();
    for input in inputs {
        let matches: Vec<PathBuf> = if input.exists() || !is_pattern(input) {
            vec![input.clone()]
        } else {
            glob::glob_with(&input.to_string_lossy(), options)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|p| p.is_file())
                .collect()
        };
        if matches.is_empty() {
            expanded.push(input.clone());
        }
        for path in matches {
            if seen.insert(path_key(&path)) {
                expanded.push(path);
            }
        }
    }
    expanded
}

fn is_pattern(path: &Path) -> bool {
    path.to_string_lossy().contains(['*', '?'])
}

fn plan(cli: &Cli, input: &Path) -> Result<Plan> {
    if is_pattern(input) && !input.exists() {
        bail!("通配符没有匹配到任何文件");
    }
    let mut head = Vec::with_capacity(512);
    File::open(input)
        .context("无法打开文件")?
        .take(512)
        .read_to_end(&mut head)
        .context("读取失败")?;
    let input_format = decode::sniff(&head).context(decode::UNSUPPORTED_INPUT)?;

    let format = explicit_format(cli).unwrap_or(match input_format {
        InputFormat::Png => OutputFormat::Png,
        InputFormat::Jpeg | InputFormat::Heic => OutputFormat::Jpeg,
        InputFormat::Webp => OutputFormat::Webp,
    });

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

/// -f 优先，其次是 -o 的扩展名
fn explicit_format(cli: &Cli) -> Option<OutputFormat> {
    match cli.format {
        FormatArg::Jpeg => Some(OutputFormat::Jpeg),
        FormatArg::Png => Some(OutputFormat::Png),
        FormatArg::Webp => Some(OutputFormat::Webp),
        FormatArg::Auto => cli.output.as_deref().and_then(OutputFormat::from_path),
    }
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

fn search_encode(encoder: &Encoder, qualities: &[u8], parallel: bool) -> Result<Vec<Vec<u8>>> {
    if let [q] = qualities {
        return Ok(vec![encoder.encode(*q, Effort::Search)?]);
    }
    if parallel {
        return qualities
            .par_iter()
            .map(|&q| encoder.encode(q, Effort::Search))
            .collect();
    }
    qualities
        .iter()
        .map(|&q| encoder.encode(q, Effort::Search))
        .collect()
}

fn process(cli: &Cli, input: &Path, plan: &Plan, report: &mut Report) -> Result<()> {
    let original = fs::read(input).context("读取失败")?;
    report.input_bytes = Some(original.len() as u64);
    let decoded = decode::decode(&original)?;
    let (width, height) = (decoded.width, decoded.height);
    report.width = Some(width);
    report.height = Some(height);

    // 原图带着要去掉的元数据，就不能拿原图顶替结果
    let strips_metadata =
        (cli.strip_icc && decoded.icc.is_some()) || (cli.strip_exif && decoded.exif.is_some());
    let icc = if cli.strip_icc { None } else { decoded.icc };
    let exif = if cli.strip_exif { None } else { decoded.exif };
    let mut encoder = match plan.format {
        OutputFormat::Jpeg => Encoder::Jpeg(JpegJob::new(
            width,
            height,
            decoded.rgba,
            cli.chroma,
            cli.background,
            icc.as_deref(),
            exif.as_deref(),
        )),
        OutputFormat::Png => Encoder::Png(PngJob::new(
            width,
            height,
            decoded.rgba,
            cli.colors,
            cli.dither.unwrap_or(1.0),
            icc.as_deref(),
            exif.as_deref(),
        )),
        OutputFormat::Webp => Encoder::Webp(WebpJob::new(
            width,
            height,
            decoded.rgba,
            icc.as_deref(),
            exif.as_deref(),
        )),
    };

    let (mut data, mut status) = match cli.max_size {
        None if cli.convert_only => {
            report.tries = 1;
            (encoder.convert()?, Status::Ok)
        }
        None => {
            let quality = cli.quality.unwrap_or(DEFAULT_QUALITY);
            report.quality = Some(quality);
            report.tries = 1;
            (encoder.encode(quality, Effort::Final)?, Status::Ok)
        }
        Some(cap) => {
            let ceiling = cli.quality.unwrap_or(plan.format.size_mode_ceiling());
            // 下限不能高于上限，否则显式给的低 -q 会被默认下限顶掉，关抖动重试也会被跳过
            let min_quality = cli
                .min_quality
                .unwrap_or(plan.format.size_mode_floor())
                .min(ceiling);
            let is_png = matches!(plan.format, OutputFormat::Png);
            // PNG 的 q100 走无损路径，和有损区间是两条体积曲线
            let split = is_png && ceiling >= 100;
            // 并行试探每个都要额外占一份量化内存；多文件时文件本身已经占满线程，
            // 大图也容易把内存顶高，这两种情况退回逐个试探。
            let parallel = split
                && cli.inputs.len() == 1
                && u64::from(width) * u64::from(height) <= 16_000_000
                && rayon::current_num_threads() >= 3;
            let order = if split {
                Order::SplitCeiling { parallel }
            } else {
                Order::Ceiling
            };
            let mut search_ceiling = ceiling;
            let mut result = fit_size(min_quality, ceiling, cap, order, |qs| {
                search_encode(&encoder, qs, parallel)
            })?;
            let mut probes = std::mem::take(&mut result.probes);
            // 抖动噪点让 PNG 很难再变小：未指定 --dither 时，达不到上限就关掉抖动再搜一次。
            // 无损和抖动无关，所以重试的上限是 99
            let ditherless_ceiling = ceiling.min(99);
            if matches!(result.fit, Fit::Unreachable { .. })
                && cli.dither.is_none()
                && ditherless_ceiling >= min_quality
                && encoder.drop_dither()
            {
                let retry = fit_size(min_quality, ditherless_ceiling, cap, Order::Floor, |qs| {
                    search_encode(&encoder, qs, false)
                })?;
                result.tries += retry.tries;
                result.fit = retry.fit;
                probes.extend(retry.probes);
                search_ceiling = ditherless_ceiling;
            }
            report.tries = result.tries;
            let (Fit::Found {
                mut quality,
                data: searched,
            }
            | Fit::Unreachable {
                mut quality,
                data: searched,
            }) = result.fit;
            // 搜索用快速档位估算，体积偏大；是否达标以定稿体积为准
            let searched_len = searched.len();
            let mut data = encoder.refine(quality, searched)?;
            let mut fits = data.len() as u64 <= cap;
            // JPEG、WebP 的估算就是定稿体积，不必复测
            let mut top = None;
            if is_png {
                let mut retests = 0;
                top = retest_top(
                    ceiling,
                    min_quality,
                    fits.then_some(quality),
                    cap,
                    &probes,
                    |q| {
                        retests += 1;
                        encoder.encode(q, Effort::Final)
                    },
                )?;
                report.tries += retests;
            }
            match top {
                Some((q, d)) => {
                    (quality, data) = (q, d);
                    fits = true;
                }
                // 定稿比估算小，更高的质量也可能放得下；无损已由复测决定，只在有损区间里爬
                None if fits && data.len() < searched_len => {
                    let (q, d, tries) = climb(quality, data, search_ceiling.min(99), cap, |q| {
                        encoder.encode(q, Effort::Final)
                    })?;
                    (quality, data) = (q, d);
                    report.tries += tries;
                }
                None => {}
            }
            let status = if fits {
                Status::Ok
            } else if cli.best_effort {
                Status::BestEffort
            } else {
                report.status = Status::SizeUnreachable;
                report.quality = Some(quality);
                report.dither = encoder.dither().filter(|_| quality < 100);
                report.output_bytes = Some(data.len() as u64);
                let hint = if is_png {
                    PNG_JPEG_HINT
                } else {
                    "可降低 --min-quality"
                };
                bail!(
                    "质量降到 {quality} 仍有 {}，超过上限 {}（{hint}，或加 --best-effort 写出最小结果）",
                    human_size(data.len() as u64),
                    human_size(cap)
                );
            };
            report.quality = Some(quality);
            (data, status)
        }
    };
    if report.quality.is_some_and(|q| q < 100) {
        report.dither = encoder.dither();
    }

    let kept_original = plan.format.same_as(plan.input_format)
        && !cli.allow_larger
        && !strips_metadata
        && data.len() >= original.len();
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
                    Status::BestEffort if r.format == Some("png") => line.push_str(&format!(
                        "  （未达到大小上限，已写出最小结果；{PNG_JPEG_HINT}）"
                    )),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_inputs_globs_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("sqz-glob-{}", std::process::id()));
        // 名字像图片的目录不应被选中
        fs::create_dir_all(dir.join("sub.png")).unwrap();
        for name in ["a.png", "b.PNG", "c.jpg"] {
            fs::write(dir.join(name), b"").unwrap();
        }
        let literal = dir.join("a.png");
        let missing = dir.join("*.webp");
        let got = expand_inputs(&[
            literal.clone(),
            dir.join("*.png"),
            missing.clone(),
            dir.join("c.jpg"),
        ]);
        fs::remove_dir_all(&dir).unwrap();

        let mut expected = vec![literal];
        if cfg!(windows) {
            expected.push(dir.join("b.PNG"));
        }
        expected.extend([missing, dir.join("c.jpg")]);
        assert_eq!(got, expected);
    }

    #[test]
    fn explicit_format_prefers_flag_over_extension() {
        let explicit = |args: &[&str]| {
            explicit_format(&Cli::try_parse_from(["sqz", "a"].iter().chain(args)).unwrap())
        };
        assert_eq!(explicit(&[]), None);
        assert_eq!(explicit(&["-o", "x.gif"]), None);
        assert_eq!(explicit(&["-o", "x.JPEG"]), Some(OutputFormat::Jpeg));
        assert_eq!(
            explicit(&["-f", "png", "-o", "x.webp"]),
            Some(OutputFormat::Png)
        );
    }
}
