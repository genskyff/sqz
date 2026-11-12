use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::{Cli, FormatArg};
use crate::decode::{self, InputFormat};
use crate::encode::OutputFormat;

pub struct Plan {
    pub input_format: InputFormat,
    pub format: OutputFormat,
    pub output: PathBuf,
}

/// PowerShell 和 cmd 不展开通配符；匹配不到的原样留给后面报错
pub fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
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

pub fn plan(cli: &Cli, input: &Path) -> Result<Plan> {
    if is_pattern(input) && !input.exists() {
        bail!("通配符没有匹配到文件");
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
        bail!("输出文件已存在：{}，加 --overwrite 覆盖", output.display());
    }
    Ok(Plan {
        input_format,
        format,
        output,
    })
}

pub fn explicit_format(cli: &Cli) -> Option<OutputFormat> {
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
    let ext = match input.extension() {
        Some(ext) if OutputFormat::from_path(input) == Some(format) => ext.to_string_lossy(),
        _ => format.extension().into(),
    };
    dir.join(format!("{stem}{suffix}.{ext}"))
}

pub fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 输出文件可能还不存在，用绝对路径判断是不是同一个
pub fn path_key(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let key = abs.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

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
    fn plans_sample_images() {
        for (name, format, output) in [
            ("sample.heic", OutputFormat::Jpeg, "sample.jpg"),
            ("sample.jpeg", OutputFormat::Jpeg, "sample.jpeg"),
            ("sample.jpg", OutputFormat::Jpeg, "sample.jpg"),
            ("sample.png", OutputFormat::Png, "sample.png"),
            ("sample.webp", OutputFormat::Webp, "sample.webp"),
        ] {
            let input = decode::sample(name);
            let cli = Cli::try_parse_from(["sqz", "a", "-d", "out", "-y"]).unwrap();
            let plan = plan(&cli, &input).unwrap();
            assert_eq!(plan.format, format, "{name}");
            assert_eq!(plan.output, Path::new("out").join(output), "{name}");
        }
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
