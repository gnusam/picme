//! JPEG XL par jxl-oxide. Le flux de rendu applique deja l'orientation du fichier.

use super::{Frame, Plan, Target, check_dims};
use crate::error::{Error, Result};
use crate::orient::Orientation;
use jxl_oxide::{AllocTracker, JxlImage, PixelFormat};
use std::path::Path;

/// Plafond des allocations de jxl-oxide (grilles f32 du rendu) : 3 Gio, de quoi rendre 200 Mpx en trois canaux.
const ALLOC_LIMIT: usize = 3 << 30;

pub(super) fn decode(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    let image = JxlImage::builder().alloc_tracker(AllocTracker::with_limit(ALLOC_LIMIT)).open(path).map_err(Error::decode)?;
    // l'en-tete donne les dimensions avant tout rendu : une image hors plafond ne coute aucune allocation
    check_dims(image.width(), image.height())?;
    let format = image.pixel_format();
    if matches!(format, PixelFormat::Cmyk | PixelFormat::Cmyka) {
        return Err(Error::Unsupported("JXL CMYK".into()));
    }
    let render = image.render_frame(0).map_err(Error::decode)?;
    let mut stream = render.stream();
    let (w, h, channels) = (stream.width(), stream.height(), stream.channels() as usize);
    check_dims(w, h)?;
    let mut samples = vec![0u8; w as usize * h as usize * channels];
    let written = stream.write_to_buffer::<u8>(&mut samples);
    if written != samples.len() {
        return Err(Error::Decode(format!("JXL : {written} echantillons sur {}", samples.len())));
    }

    let has_alpha = matches!(format, PixelFormat::Graya | PixelFormat::Rgba);
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for px in samples.chunks_exact(channels) {
        match channels {
            1 => rgba.extend_from_slice(&[px[0], px[0], px[0], 255]),
            2 => rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]),
            3 => rgba.extend_from_slice(&[px[0], px[1], px[2], 255]),
            _ => rgba.extend_from_slice(&px[..4]),
        }
    }
    Plan::new(w, h, Orientation::Normal, target, max_full_bytes).finish(rgba, w, h, has_alpha, Orientation::Normal)
}
