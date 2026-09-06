# Test fixtures

## `page.jpg`

Generated in this repository, not taken from anywhere, so its provenance is unambiguous
and no licence question arises.

- 160 x 240, greyscale content in three RGB bands, produced by
  `banded(160, 240, Channels::Rgb)` in
  `tests/page_codec.rs`: hard black-on-white vertical edges over the top third, a
  horizontal gradient across the middle, and a flat mid-grey across the bottom. Those are
  the three things manga content stresses — line art, screentone, and empty page.
- Encoded by this crate's own encoder with `EncodeSettings::default()`: quality 90,
  entropy-coding optimisation on, progressive on, DCT method `ifast`.
- Verified to be readable by a decoder that is not this crate's, so the round-trip test is
  not merely reading back its own output: macOS `sips` and `file(1)` both report a valid
  progressive JPEG, 160x240, three components.

To regenerate it, encode `banded(160, 240, Channels::Rgb)` with `EncodeSettings::default()`
and write the bytes here. The output is not expected to be byte-identical across mozjpeg
releases, so replace the file wholesale rather than expecting a clean diff. Quality 90 is
also the range where the encoder's forced-baseline quantisation is byte-identical to
`Compress::set_quality`, so this file is unchanged by that fix.

## Archive fixtures

There are none committed, deliberately. The pipeline tests build their archives at run time
through `tests/support/mod.rs`.

An archive of pages wide enough to exercise normalisation is two orders of magnitude larger
than `page.jpg`: three 1520x2150 pages come to about 216 KB, and the fixtures for the
peak-memory measurement are 7 MB and 72 MB. Committing the first pair and generating the
second would also have meant two provenance stories for one kind of fixture.

Generating them is not self-referential. The encoder the pages come from is the one
`page.jpg` verifies, and `page.jpg`'s own validity was established with a decoder that is
not this crate's; the zip framing is verified separately in `tests/archive_source.rs`.

The page pattern is described beside `support::page`. Two of its properties are load-bearing
rather than decorative: the strokes are anti-aliased, because a hard black-to-white edge is
what a windowed-sinc resampler handles worst — 8-pixel hard stripes downscaled by 0.84
re-encoded to 2.7 times the input's bytes — and they are sparse, because a manga page is
mostly paper.

The fixtures for acceptance criterion 5 are written on demand:

```sh
CAR_FIXTURE_DIR=/tmp/car-memory \
  cargo test --locked --release --test pipeline -- --ignored --nocapture
```
