//! PNG, WebP et TIFF par la crate `image` : decode complet, puis reduction.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::media::MediaKind;
use crate::orient::Orientation;
use image::{ImageFormat, ImageReader};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub(super) fn decode(path: &Path, kind: MediaKind, target: Target, max_full_bytes: usize) -> Result<Frame> {
    let format = match kind {
        MediaKind::Png => ImageFormat::Png,
        MediaKind::Webp => ImageFormat::WebP,
        MediaKind::Tiff => ImageFormat::Tiff,
        other => return Err(Error::Unsupported(format!("{other:?} n'est pas un format raster"))),
    };
    let img = ImageReader::with_format(BufReader::new(File::open(path)?), format).decode().map_err(Error::decode)?;
    let has_alpha = img.color().has_alpha();
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    // `image` n'applique jamais l'orientation : l'EXIF (eXIf du PNG, WebP, tag TIFF) est lu a part
    let orient = Orientation::read(path);
    let plan = Plan::new(w, h, orient, target, max_full_bytes);
    plan.finish(rgba.into_raw(), w, h, has_alpha, orient)
}
