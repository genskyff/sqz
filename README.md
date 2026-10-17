# sqz

纯 Rust 实现的图片压缩工具。输入 PNG、JPEG、HEIC，输出 JPEG 或 PNG，可以指定质量，也可以只给文件大小上限，由工具反推质量。默认参数参考 [Squoosh](https://github.com/GoogleChromeLabs/squoosh)。

## 安装

从 [Releases](../../releases) 下载 Windows x64 版本，解压后把 `sqz.exe` 放进 `PATH`。

也可以用 cargo 安装：

```shell
cargo install --git https://github.com/genskyff/sqz --locked
```

## 用法

```shell
sqz photo.jpg                            # 默认 q75，输出 photo.min.jpg
sqz IMG_0001.HEIC -s 500KB               # 转成 JPEG，不超过 500KB
sqz *.png *.jpg -q 90 -s 1MB -d out      # 质量不高于 90，且不超过 1MB
sqz icon.png -q 100 -o icon.opt.png      # PNG 无损压缩
sqz a.png b.heic -s 300KB --json         # 输出 JSON
```

完整参数见 `sqz --help`。

## 说明

- `-s` 会搜索不超过上限的最高质量；除非加 `--best-effort`，输出一定不超过上限
- PNG 降质靠减少颜色，照片类图片压不到目标大小时改用 `-f jpeg`
- EXIF 方向会应用到像素上，EXIF 本身不保留；ICC 默认保留

## JSON 输出

`--json` 输出一个数组，每个输入一项，没有值的字段会省略：

| 字段 | 说明 |
|---|---|
| `input`、`output` | 路径 |
| `status` | `ok`、`kept_original`、`best_effort`、`size_unreachable`、`error` |
| `format` | `jpeg` 或 `png` |
| `width`、`height` | 尺寸 |
| `input_bytes`、`output_bytes` | 字节数；`size_unreachable` 时是能压到的最小值，文件不会写出 |
| `quality`、`dither` | 最终使用的参数 |
| `tries` | 编码次数 |
| `error` | 错误信息 |
| `elapsed_ms` | 耗时 |

退出码：0 成功，1 有文件失败或达不到大小上限，2 参数错误。

## 限制

- HEIC 只取主图的 8-bit 画面，HDR 增益图会丢弃
- HEIC 没有 ICC、只用 nclx 标注 Display P3 时，颜色会偏淡
- 16-bit PNG 会降为 8-bit
