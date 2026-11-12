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
    let mut decoded = decode::decode(&original)?;
    let resized_from = cli.max_edge.and_then(|edge| decoded.shrink_to_edge(edge));
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
        && resized_from.is_none()
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
    report.resized = resized_from.map(|from| (from, (width, height)));
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
    // 下限高于上限的话，显式给的低 -q 会被默认下限顶掉
    let min_quality = cli
        .min_quality
        .unwrap_or(format.size_mode_floor())
        .min(ceiling);
    let is_png = matches!(format, OutputFormat::Png);
    // PNG 的 q100 是无损，和有损区间是两条体积曲线
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
    // 抖动噪点让 PNG 压不小，关掉再搜；无损与抖动无关，上限取 99
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
    // 搜索时的估算偏大，是否达标以定稿为准
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
        // 无损搜索时已有定论，只复测 99；上限有损时复测它和下一档
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
        // 定稿比估算小，更高的质量也可能放得下
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::Parser;

    use super::*;
    use crate::plan::plan;

    /// 噪点图：体积随像素数和质量明显变化
    fn noise(format: OutputFormat, width: u32, height: u32, quality: u8) -> Vec<u8> {
        let mut state = 5u32;
        let rgba = (0..width * height)
            .flat_map(|_| {
                state = state.wrapping_mul(1103515245).wrapping_add(12345) & 0x7fff_ffff;
                let v = (state >> 16) as u8;
                [v, v.wrapping_mul(3), 255 - v, 255]
            })
            .collect();
        let cli = Cli::try_parse_from(["sqz", "x"]).unwrap();
        Encoder::new(format, &cli, width, height, rgba, Metadata::default())
            .encode(quality, Effort::Final)
            .unwrap()
    }

    /// 在各自的临时目录里处理 `input`，返回报告和写出的文件
    fn run(name: &str, input: &[u8], args: &[&str]) -> (Report, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("sqz-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in");
        fs::write(&path, input).unwrap();
        let args = [OsString::from("sqz"), path.clone().into_os_string()]
            .into_iter()
            .chain(args.iter().map(OsString::from));
        let cli = Cli::try_parse_from(args).unwrap();
        let plan = plan(&cli, &path).unwrap();
        let mut report = Report::new(&path);
        let outcome = process(&cli, &path, &plan, &mut report);
        let output = fs::read(&plan.output);
        fs::remove_dir_all(&dir).unwrap();
        outcome.unwrap();
        (report, output.unwrap())
    }

    #[test]
    fn max_edge_shrinks_before_fitting_max_size() {
        let input = noise(OutputFormat::Png, 96, 64, 100);
        let (full, _) = run("edge-size", &input, &["-f", "webp", "-s", "3000"]);
        let (report, output) = run(
            "edge-size",
            &input,
            &["-f", "webp", "-s", "3000", "-e", "72"],
        );
        assert_eq!(report.status, Status::Ok);
        assert_eq!(report.resized, Some(((96, 64), (72, 48))));
        assert!(output.len() <= 3000);
        let decoded = decode::decode(&output).unwrap();
        assert_eq!((decoded.width, decoded.height), (72, 48));
        // 缩小后仍要搜索才放得下，但像素少了，同样的上限下质量更高
        assert!(report.tries > 1);
        assert!(report.quality > full.quality);
    }

    #[test]
    fn resized_output_never_falls_back_to_the_original() {
        let input = noise(OutputFormat::Jpeg, 64, 48, 1);
        // 没缩小时照旧：同格式压不小就保留原图
        for args in [&["-q", "100"][..], &["-q", "100", "-e", "64"]] {
            let (report, output) = run("edge-original", &input, args);
            assert_eq!(report.status, Status::KeptOriginal);
            assert_eq!(report.resized, None);
            assert_eq!(output, input);
        }

        let (report, output) = run("edge-original", &input, &["-q", "100", "-e", "60"]);
        assert!(output.len() >= input.len());
        assert_eq!(report.status, Status::Ok);
        assert_eq!(report.resized, Some(((64, 48), (60, 45))));
        let decoded = decode::decode(&output).unwrap();
        assert_eq!((decoded.width, decoded.height), (60, 45));
    }
}
