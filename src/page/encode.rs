//! JPEG encode, through mozjpeg's compressor.
//!
//! This is the half that earns the dependency: trellis quantisation, scan optimisation,
//! and mozjpeg's tuned quantisation tables produce a smaller file than libjpeg-turbo at
//! the same visual quality. Quality therefore stays high and the size reduction comes
//! from the resize, not from quantising line art into ringing.

use std::io;
use std::panic;

use mozjpeg::{ColorSpace, Compress};

use super::{DctMethod, PageError, PageErrorKind, RgbImage, unwind_reason};

/// The four encoder settings the tool exposes, matching the Go implementation's flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodeSettings {
    /// libjpeg quality, 1 to 100.
    pub quality: u8,
    /// Optimise the entropy-coding tables. Costs a pass, saves bytes.
    pub optimize_coding: bool,
    /// Emit a progressive rather than a baseline file.
    pub progressive: bool,
    pub dct_method: DctMethod,
}

impl Default for EncodeSettings {
    /// Quality 90 with optimisation and progressive on, as the Go implementation
    /// defaults.
    fn default() -> Self {
        Self {
            quality: 90,
            optimize_coding: true,
            progressive: true,
            dct_method: DctMethod::default(),
        }
    }
}

/// Encodes `source` and returns the JPEG bytes.
///
/// Returns bytes rather than writing to a path: the pipeline hands them to an ordering
/// writer that puts them into the output archive, and never touches the filesystem for
/// an intermediate.
///
/// # Errors
///
/// [`PageErrorKind::Encode`] when libjpeg refuses the image. As with decoding, libjpeg
/// reports a fatal error by unwinding out of C, and that unwind is caught here.
pub fn encode(
    name: &str,
    source: &RgbImage,
    settings: EncodeSettings,
) -> Result<Vec<u8>, PageError> {
    panic::catch_unwind(|| encode_jpeg(source, settings))
        .map_err(|payload| {
            PageError::new(name, PageErrorKind::Encode(unwind_reason(payload.as_ref())))
        })?
        .map_err(|error| PageError::new(name, PageErrorKind::Encode(error.to_string())))
}

/// The part that may unwind. Kept separate so the `catch_unwind` closure stays trivial.
fn encode_jpeg(source: &RgbImage, settings: EncodeSettings) -> io::Result<Vec<u8>> {
    let mut compress = Compress::new(ColorSpace::JCS_RGB);
    compress.set_size(source.width() as usize, source.height() as usize);

    // Quality first: `set_quality` installs quantisation tables, and the settings below
    // must not be overwritten by it.
    compress.set_quality(f32::from(settings.quality));
    compress.set_optimize_coding(settings.optimize_coding);

    // Not `set_progressive_mode()`. mozjpeg's compression profile already emits a
    // progressive file, built from its own optimised scan script, and that call — libjpeg's
    // `jpeg_simple_progression` — can only replace the script, never remove it. Measured on
    // the committed fixture: doing nothing and calling it both give 1807 bytes with SOF2,
    // while clearing the script gives 1846 bytes with SOF0. The script is therefore what
    // gets toggled, in both directions rather than only one, so the setting does not depend
    // on the library's default staying what it is today.
    compress.set_optimize_scans(settings.progressive);
    // Reachable only through the fork; see `[patch.crates-io]` in `Cargo.toml`.
    compress.set_dct_method(settings.dct_method.into());

    let mut started = compress.start_compress(Vec::new())?;
    started.write_scanlines(source.pixels())?;
    started.finish()
}

#[cfg(test)]
mod tests {
    use super::EncodeSettings;
    use crate::page::DctMethod;

    #[test]
    fn defaults_match_the_go_implementation() {
        let settings = EncodeSettings::default();
        assert_eq!(settings.quality, 90);
        assert!(settings.optimize_coding);
        assert!(settings.progressive);
        assert_eq!(settings.dct_method, DctMethod::IntegerFast);
    }
}
