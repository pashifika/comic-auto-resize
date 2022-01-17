This is a manga archiver file auto-resize tool
==============================================

## Image format support

| Format | Decoder | Encoder | Test  | Info                                                     |
|--------|---------|---------|-------|----------------------------------------------------------|
| jpeg   | true    | true    | local | ues [mozjpeg v4.0.3](https://github.com/mozilla/mozjpeg) |
| png    | true    | false   | local | go std lib                                               |
| bmp    | true    | false   | local | go std lib                                               |
| webp   | true    | false   | local | go std lib(incomplete? bug with mozjpeg encoder)         |

Usage:
------

```
  comic-auto-resize [OPTIONS] (archiver file / directory)
Version: v1.0.1

Application Options:
      --charset=                decode zip file charset. (default: ja,zh)
      --delete-org              enable delete original file. (default: false)
  -o, --out=                    set output file path (default is add suffix '_resize' to file name).
  -r, --ratio=                  set resize ratio. (default: 70)
  -q, --quality=                set encoder quality. (default: 90)
      --resize-mode=            set resize interpolation mode.
                                Supported:
                                nearest-neighbor, bilinear, bicubic,
                                mitchell-netravali, lanczos2, lanczos3. (default: lanczos3)

Jpeg Options:
      --optimizer               perform optimization of entropy encoding parameters. (default: true)
      --progressive             create progressive JPEG file. (default: true)
      --dct=[float|ifast|islow] set JPEG encoder DCT/IDCT method.
                                FLOAT is floating-point: accurate, fast on fast HW.
                                IFAST is faster, less accurate integer method.
                                ISLOW is slow but accurate integer algorithm. (default: ifast)

Developer Options:
      --show-time               enable show execution time. (default: false)
      --debug                   enable debug mode. (default: false)

Help Options:
  -h, --help                    Show this help message
```
