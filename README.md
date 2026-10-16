# sqz

纯 Rust 的图片压缩命令行工具，输入 PNG / JPEG / HEIC，输出压缩后的 JPEG 或 PNG。可以按质量压缩，也可以给定大小上限、反推出不超限的最高质量。参数和默认值参考了 [Squoosh](https://github.com/GoogleChromeLabs/squoosh)。

## 构建

```shell
cargo build --release
```

## 用法

```shell
# 按质量压缩（默认 q75），输出 photo.min.jpg
sqz photo.jpg

# HEIC 转 JPEG，大小不超过 500KB，自动反推质量
sqz IMG_0001.HEIC -s 500KB

# 质量上限 90，同时不超过 1MB；批量输出到 out/
sqz *.png *.jpg -q 90 -s 1MB -d out

# PNG 无损优化
sqz icon.png -q 100 -o icon.opt.png

# 给脚本或 agent 用：结果以 JSON 输出到 stdout
sqz a.png b.heic -s 300KB -d out --json
```

完整参数见 `sqz --help`。

## 行为约定

- **输出格式**：默认 PNG→PNG，JPEG→JPEG，HEIC→JPEG；`-f jpeg|png` 强制指定；用 `-o` 时按扩展名推断。
- **质量刻度**：JPEG 用 mozjpeg 刻度（`-q 75` 与 Squoosh 默认值的体积相近）；PNG 的 `-q` 是 imagequant 的目标质量，`-q 100` 表示无损。
- **目标大小**：`-s` 在 `[--min-quality, 上限]` 区间内对整数质量二分，只采纳真正不超过上限的结果。上限取 `-q`，未给 `-q` 时 JPEG 为 95、PNG 为 100（先试无损）。PNG 开着抖动达不到上限时，会关掉抖动再搜一次（显式给了 `--dither` 则不会）。
- **达不到上限**：默认不写文件、退出码为 1；加 `--best-effort` 则写出能做到的最小结果。
- **不变大**：同格式输出不比原图小时，直接复制原图（`status: kept_original`）；加 `--allow-larger` 关闭此行为。
- **元数据**：EXIF 方向会应用到像素上，然后丢弃全部 EXIF（包括 GPS）；ICC 色彩配置默认保留，`--strip-icc` 可去掉。
- **安全**：默认不覆盖已有文件，原地覆盖输入也需要 `--overwrite`；多个输入写到同一输出路径时直接报错；先写临时文件再改名，不会留下写了一半的文件。

## JSON 输出

每个输入对应一项，没有值的字段不输出：

| 字段 | 说明 |
|---|---|
| `input` / `output` | 输入、输出路径 |
| `status` | `ok`、`kept_original`、`best_effort`、`size_unreachable`、`error` |
| `format` | 输出格式 `jpeg` / `png` |
| `width` / `height` | 像素尺寸（已按 EXIF 方向旋转） |
| `input_bytes` / `output_bytes` | 字节数；`size_unreachable` 时为能做到的最小值 |
| `quality` / `dither` | 最终质量、PNG 抖动强度 |
| `tries` | 编码次数 |
| `error` | 错误说明 |
| `elapsed_ms` | 耗时 |

退出码：0 表示全部成功；1 表示有文件失败或达不到大小上限；2 表示参数错误。

## 实现

| 环节 | 依赖 |
|---|---|
| PNG/JPEG 解码 | `image`（png、zune-jpeg） |
| HEIC 解码 | `heic` |
| JPEG 编码 | `zenjpeg`（jpegli 的 Rust 移植） |
| PNG 量化 | `imagequant` |
| PNG 写出 | 自己实现：色彩类型精简、调色板排序与位深缩减、滤波器试选，deflate 用 `miniz_oxide` |

## 已知限制

- HEIC 只解码主图的 8-bit SDR 画面，HDR 增益图会丢弃。如果 HEIC 只用 nclx 标注 Display P3、不带 ICC，输出会按 sRGB 解读，颜色会偏淡。
- 16-bit PNG 会降为 8-bit。
- 不保留 EXIF（拍摄时间等）。
