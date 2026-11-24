//! PNG, WebP, TIFF, GIF (premiere image), BMP, ICO, QOI et PNM par la crate `image` : decode complet, puis reduction.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::media::MediaKind;
use crate::orient::Orientation;
use image::{ImageFormat, ImageReader};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

fn format_of(kind: MediaKind) -> Result<ImageFormat> {
    match kind {
        MediaKind::Png => Ok(ImageFormat::Png),
        MediaKind::Webp => Ok(ImageFormat::WebP),
        MediaKind::Tiff => Ok(ImageFormat::Tiff),
        MediaKind::Gif => Ok(ImageFormat::Gif),
        MediaKind::Bmp => Ok(ImageFormat::Bmp),
        MediaKind::Ico => Ok(ImageFormat::Ico),
        MediaKind::Qoi => Ok(ImageFormat::Qoi),
        MediaKind::Pnm => Ok(ImageFormat::Pnm),
        other => Err(Error::Unsupported(format!("{other:?} n'est pas un format raster"))),
    }
}

/// Dimensions de l'image entiere, orientation appliquee, lues dans l'en-tete sans decoder les pixels.
pub(super) fn probe(path: &Path, kind: MediaKind) -> Option<(u32, u32)> {
    let reader = ImageReader::with_format(BufReader::new(File::open(path).ok()?), format_of(kind).ok()?);
    let (w, h) = reader.into_dimensions().ok()?;
    Some(Orientation::read(path).displayed_dims(w, h))
}

pub(super) fn decode(path: &Path, kind: MediaKind, target: Target, max_full_bytes: usize) -> Result<Frame> {
    let format = format_of(kind)?;
    let img = ImageReader::with_format(BufReader::new(File::open(path)?), format).decode().map_err(Error::decode)?;
    let has_alpha = img.color().has_alpha();
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    // `image` n'applique jamais l'orientation : l'EXIF (eXIf du PNG, WebP, tag TIFF) est lu a part
    let orient = Orientation::read(path);
    let plan = Plan::new(w, h, orient, target, max_full_bytes);
    plan.finish(rgba.into_raw(), w, h, has_alpha, orient)
}
