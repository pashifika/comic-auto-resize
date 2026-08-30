# Test fixtures

## `page.jpg`

Generated in this repository, not taken from anywhere, so its provenance is unambiguous
and no licence question arises.

- 160 x 240, greyscale content in three RGB bands, produced by `banded(160, 240)` in
  `tests/page_codec.rs`: hard black-on-white vertical edges over the top third, a
  horizontal gradient across the middle, and a flat mid-grey across the bottom. Those are
  the three things manga content stresses — line art, screentone, and empty page.
- Encoded by this crate's own encoder with `EncodeSettings::default()`: quality 90,
  entropy-coding optimisation on, progressive on, DCT method `ifast`.
- Verified to be readable by a decoder that is not this crate's, so the round-trip test is
  not merely reading back its own output: macOS `sips` and `file(1)` both report a valid
  progressive JPEG, 160x240, three components.

To regenerate it, encode `banded(160, 240)` with `EncodeSettings::default()` and write the
bytes here. The output is not expected to be byte-identical across mozjpeg releases, so
replace the file wholesale rather than expecting a clean diff.
