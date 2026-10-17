# sqz

纯 Rust 实现的图片压缩工具。输入 PNG、JPEG、HEIC，输出 JPEG 或 PNG，可以指定质量，也可以只给文件大小上限，由工具反推质量。默认参数参考 [Squoosh](https://github.com/GoogleChromeLabs/squoosh)。

## 安装

从 [Releases](../../releases) 下载 Windows x64 版本，解压后把 `sqz.exe` 放进 `PATH`。

也可以用 cargo 安装：

```shell
cargo install --git https://github.com/genskyff/sqz --locked
```

## 构建

```shell
cargo build --release
```

## 用法

```shell
sqz photo.jpg                            # 默认 q75，输出 photo.min.jpg
sqz IMG_0001.HEIC -s 500KB               # 转成 JPEG，不超过 500KB
sqz *.png *.jpg -q 90 -s 1MB -d out      # 质量不高于 90，且不超过 1MB
sqz icon.png -q 100 -o icon.opt.png      # PNG 无损压缩
sqz a.png b.heic -s 300KB --json         # 输出 JSON
```

其他参数见 `sqz --help`。

## 规则

- 参数含 `*` 或 `?` 时由 sqz 自己展开，PowerShell 和 cmd 下也能用。Windows 上不区分大小写
- 输出格式跟随输入，HEIC 输出 JPEG。可以用 `-f` 指定，或由 `-o` 的扩展名决定
- 默认输出到原目录，文件名加 `.min`；用 `-d` 时不加后缀，`--suffix` 可以改
- JPEG 的 `-q` 和 mozjpeg 刻度接近。PNG 的 `-q` 是 imagequant 的质量，100 为无损
- `-s` 用二分搜索找质量，结果一定不超过上限。不给 `-q` 时，JPEG 从 95 开始试，PNG 从无损开始试。PNG 压不下去时会关掉抖动再试一次，除非指定了 `--dither`
- 达不到上限就不写文件，返回 1。加 `--best-effort` 会输出能压到的最小结果
- 同格式输出不比原图小时，保留原图。`--allow-larger` 可关闭这个行为
- 按 EXIF 方向旋转像素后丢弃 EXIF，保留 ICC，`--strip-icc` 可以去掉
- 不覆盖已有文件，除非加 `--overwrite`。多个输入写到同一个输出路径时直接报错

## JSON 输出

`--json` 输出一个数组，每个输入一项，没有值的字段会省略：

| 字段 | 说明 |
|---|---|
| `input`、`output` | 路径 |
| `status` | `ok`、`kept_original`、`best_effort`、`size_unreachable`、`error` |
| `format` | `jpeg` 或 `png` |
| `width`、`height` | 尺寸 |
| `input_bytes`、`output_bytes` | 字节数 |
| `quality`、`dither` | 最终使用的参数 |
| `tries` | 编码次数 |
| `error` | 错误信息 |
| `elapsed_ms` | 耗时 |

`size_unreachable` 时不写文件，`output_bytes` 是能压到的最小值。

退出码：0 成功，1 有文件失败或达不到大小上限，2 参数错误。

## 依赖

| 用途 | crate |
|---|---|
| PNG、JPEG 解码 | `image` |
| HEIC 解码 | `heic` |
| JPEG 编码 | `zenjpeg` |
| PNG 量化 | `imagequant` |
| deflate | `flate2`（`zlib-rs` 后端） |

PNG 文件的写出部分是自己实现的。

## 限制

- HEIC 只取主图的 8-bit 画面，HDR 增益图会丢弃
- HEIC 没有 ICC、只用 nclx 标注 Display P3 时，颜色会偏淡
- 16-bit PNG 会降为 8-bit
- 不保留 EXIF
