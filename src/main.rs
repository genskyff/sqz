mod cli;
mod decode;
mod encode;
mod meta;
mod plan;
mod process;
mod report;
mod search;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Result, anyhow};
use clap::Parser;
use rayon::prelude::*;

use cli::Cli;
use plan::{Plan, expand_inputs, explicit_format, path_key, plan};
use process::process;
use report::{Report, print_human, print_json};

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() -> ExitCode {
    let mut cli = Cli::parse();
    cli.inputs = expand_inputs(&cli.inputs);
    if cli.output.is_some() && cli.inputs.len() > 1 {
        eprintln!("错误：-o 只能用于单个输入，多个输入请用 -d");
        return ExitCode::from(2);
    }
    if cli.convert_only && explicit_format(&cli).is_none() {
        eprintln!("错误：--convert-only 须用 -f 或 -o 的扩展名指定格式");
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
        print_json(&reports);
    } else {
        print_human(&reports);
    }

    if reports.iter().any(Report::failed) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
