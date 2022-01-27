This is a manga compressed file auto-resize tool
================================================

Are you still bothered by the size of your comic compressed files?<br>
Still doing the tedious operation of decompressing files -> other software processing images -> repacking compressed files?<br>
This tool will do the above work for you, from the open file all processing is done in memory!<br>
(Make sure your PC / Mac has enough memory for large files.)


## Image format support

| Format | Decoder | Encoder | Test  | Info                                                     |
|--------|---------|---------|-------|----------------------------------------------------------|
| jpeg   | true    | true    | local | ues [mozjpeg v4.0.3](https://github.com/mozilla/mozjpeg) |
| png    | true    | false   | local | go std lib                                               |
| bmp    | true    | false   | local | go std lib                                               |
| webp   | true    | false   | local | go std lib(incomplete? bug with mozjpeg encoder)         |


## Compressed format support

| Format | Test  | Charset | Decoder | Encoder | Password | Info                                                 |
|--------|-------|---------|---------|---------|----------|------------------------------------------------------|
| zip    | local | true    | true    | true    | false    | used go std                                          |
| rar    | local | false   | true    | false   | false    | [rardecode/v2](https://github.com/nwaples/rardecode) |

Rar file password is support for the next version.

Usage:
------

```
  comic-auto-resize [OPTIONS] (compressed file / directory)
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
