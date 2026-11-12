# sqz

纯 Rust 实现的图片压缩工具，支持 PNG、JPEG、HEIC、WebP，默认参数参考 [Squoosh](https://github.com/GoogleChromeLabs/squoosh)。

## 安装

从 [Releases](../../releases) 下载 Windows x64 版本，把 `sqz.exe` 放进 `PATH`；或者用 cargo 安装：

```shell
cargo install --git https://github.com/genskyff/sqz --locked
```

## 用法

```shell
sqz photo.jpg                            # 默认 q75，输出 photo.min.jpg
sqz IMG_0001.HEIC -s 500KB               # 转成 JPEG，不超过 500KB
sqz *.png *.jpg -q 90 -s 1MB -d out      # 质量不高于 90，且不超过 1MB
sqz icon.png -q 100 -o icon.opt.png      # PNG 无损压缩
sqz shot.png --convert-only -f webp      # 只转格式不降质，输出无损 WebP
sqz a.png b.heic -s 300KB --json         # 输出 JSON
```

完整参数见 `sqz --help`。

## JSON 输出

`--json` 输出一个数组，每个输入一项，没有值的字段省略：`input`、`output`、`status`、`format`、`width`、`height`、`input_bytes`、`output_bytes`、`quality`、`dither`、`tries`、`error`、`elapsed_ms`。

- `status`：`ok`、`kept_original`、`best_effort`、`size_unreachable`、`error`
- `size_unreachable` 时不写文件，`output_bytes` 是能压到的最小值
- 退出码：0 成功，1 有文件失败或达不到大小上限，2 参数错误

## 说明

- EXIF、XMP、ICC 默认保留；方向会应用到像素上，元数据里的方向、尺寸和 MWG 区域标注随之更新
- 输出 JPEG 时，超过 64KB 的 EXIF 或 XMP 会被丢弃
- HEIC 只取主图的 8-bit 画面，HDR 增益图会丢弃
- HEIC 没有 ICC、只用 nclx 标注 Display P3 时，颜色会偏淡
- 16-bit PNG 会降为 8-bit
