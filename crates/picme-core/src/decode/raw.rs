//! RAW (feature `raw`) par rawler : l'image JPEG embarquee par l'appareil (la plus grande disponible), pas un
//! developpement du capteur. Sans la feature, `decode` rend `Unsupported` comme avant.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::orient::Orientation;
use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;
use std::path::Path;

pub(super) fn decode(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    let source = RawSource::new(path)?;
    let decoder = rawler::get_decoder(&source).map_err(Error::decode)?;
    let params = RawDecodeParams::default();
    // du plus grand au plus petit : un RAW n'embarque pas toujours les trois
    let img = decoder
        .full_image(&source, &params)
        .ok()
        .flatten()
        .or_else(|| decoder.preview_image(&source, &params).ok().flatten())
        .or_else(|| decoder.thumbnail_image(&source, &params).ok().flatten())
        .ok_or_else(|| Error::Unsupported("RAW sans image embarquee".into()))?;
    let orient = decoder
        .raw_metadata(&source, &params)
        .ok()
        .and_then(|m| m.exif.orientation)
        .and_then(|v| Orientation::from_exif(v.into()))
        .unwrap_or(Orientation::Normal);
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Plan::new(w, h, orient, target, max_full_bytes).finish(rgba.into_raw(), w, h, false, orient)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    #[test]
    fn a_file_that_is_not_a_raw_is_an_error_not_a_panic() {
        let tmp = TmpDir::new("raw-garbage");
        let p = tmp.path().join("x.dng");
        std::fs::write(&p, b"pas un RAW du tout, juste du texte").unwrap();
        assert!(decode(&p, Target::Preview, usize::MAX).is_err());
    }
}
