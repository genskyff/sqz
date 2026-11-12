use std::path::{Path, PathBuf};
use std::{fs, iter};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::cli::Cli;
use crate::decode;
use crate::encode::{Effort, Encoder, OutputFormat};
use crate::meta::Metadata;
use crate::plan::{Plan, same_file};
use crate::report::{PNG_JPEG_HINT, Report, Status, human_size};
use crate::search::{Fit, Order, climb, climb_start, fit_size, retest_top};

const DEFAULT_QUALITY: u8 = 75;

fn encode_all(
    encoder: &Encoder,
    qualities: &[u8],
    effort: Effort,
    parallel: bool,
) -> Result<Vec<Vec<u8>>> {
    let encode = |&q: &u8| encoder.encode(q, effort);
    if parallel {
        qualities.par_iter().map(encode).collect()
    } else {
        qualities.iter().map(encode).collect()
    }
}

pub fn process(cli: &Cli, input: &Path, plan: &Plan, report: &mut Report) -> Result<()> {
    let original = fs::read(input).context("读取失败")?;
    report.input_bytes = Some(original.len() as u64);
    let decoded = decode::decode(&original)?;
    let (width, height) = (decoded.width, decoded.height);

    // 原图带着要去掉的元数据，就不能拿原图顶替结果
    let (strip_icc, strip_exif, strip_xmp) = cli.strips();
    let strips_metadata = (strip_icc && decoded.icc.is_some())
        || (strip_exif && decoded.exif.is_some())
        || (strip_xmp && decoded.xmp.is_some());
    let meta = Metadata {
        icc: decoded.icc.as_deref().filter(|_| !strip_icc),
        exif: decoded.exif.as_deref().filter(|_| !strip_exif),
        xmp: decoded.xmp.as_deref().filter(|_| !strip_xmp),
    };
    let mut encoder = Encoder::new(plan.format, cli, width, height, decoded.rgba, meta);

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
            let pixels = u64::from(width) * u64::from(height);
            fit_to_cap(cli, plan.format, &mut encoder, pixels, cap, report)?
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
    report.format = Some(plan.format);
    report.output = Some(plan.output.display().to_string());
    report.output_bytes = Some(data.len() as u64);
    Ok(())
}

fn fit_to_cap(
    cli: &Cli,
    format: OutputFormat,
    encoder: &mut Encoder,
    pixels: u64,
    cap: u64,
    report: &mut Report,
) -> Result<(Vec<u8>, Status)> {
    let ceiling = cli.quality.unwrap_or(format.size_mode_ceiling());
    // 下限不能高于上限，否则显式给的低 -q 会被默认下限顶掉，关抖动重试也会被跳过
    let min_quality = cli
        .min_quality
        .unwrap_or(format.size_mode_floor())
        .min(ceiling);
    let is_png = matches!(format, OutputFormat::Png);
    // PNG 的 q100 走无损路径，和有损区间是两条体积曲线
    let split = is_png && ceiling >= 100;
    // 并行试探各占一份编码内存；多文件时线程已被占满，大图内存吃紧，都逐个试
    let parallel =
        cli.inputs.len() == 1 && pixels <= 16_000_000 && rayon::current_num_threads() >= 3;
    let probe = Effort::Search { cap };
    let order = if split {
        Order::SplitCeiling { parallel }
    } else {
        Order::Ceiling
    };
    let mut search_ceiling = ceiling;
    let mut result = fit_size(min_quality, ceiling, cap, order, parallel, |qs| {
        encode_all(encoder, qs, probe, parallel)
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
        let retry = fit_size(
            min_quality,
            ditherless_ceiling,
            cap,
            Order::Floor,
            parallel,
            |qs| encode_all(encoder, qs, probe, parallel),
        )?;
        result.tries += retry.tries;
        result.fit = retry.fit;
        probes.extend(retry.probes);
        search_ceiling = ditherless_ceiling;
    }
    report.tries = result.tries;
    let found = matches!(result.fit, Fit::Found { .. });
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
    let hi = search_ceiling.min(99);
    if parallel && is_png && found {
        // 定稿和往上爬的头两步一起编码；PNG 编码器记着结果，后面直接取用
        let ahead: Vec<u8> = iter::once(quality)
            .chain(climb_start(quality, hi))
            .collect();
        if ahead.len() > 1 {
            encode_all(encoder, &ahead, Effort::Final, parallel)?;
        }
    }
    let mut data = encoder.refine(quality, searched, cap)?;
    let mut fits = data.len() as u64 <= cap;
    // JPEG、WebP 的估算就是定稿体积，不必复测
    let mut top = None;
    if is_png {
        // 无损的结论搜索时已确定，上限是无损时只复测 99；上限有损时复测它和下一档
        let lowest = if split { 99 } else { ceiling.saturating_sub(1) };
        let candidates = lowest.max(min_quality)..=ceiling.min(99);
        let gap = searched_len as f64 / data.len() as f64;
        let mut retests = 0;
        top = retest_top(
            candidates,
            fits.then_some(quality),
            cap,
            gap,
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
        // 定稿比估算小，更高的质量也可能放得下；只在有损区间里爬
        None if fits && data.len() < searched_len => {
            let (q, d, tries) = climb(quality, data, hi, cap, parallel, |qs| {
                encode_all(encoder, qs, Effort::Final, parallel)
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
        let hint = if is_png {
            PNG_JPEG_HINT
        } else {
            "可降低 --min-quality"
        };
        bail!(
            "质量降到 {quality} 仍有 {}，超过上限 {}；{hint}，或加 --best-effort 写出最小结果",
            human_size(data.len() as u64),
            human_size(cap)
        );
    };
    report.quality = Some(quality);
    Ok((data, status))
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
