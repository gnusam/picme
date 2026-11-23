//! HEIC et AVIF par libheif. libheif applique lui-meme irot, imir et clap : l'orientation EXIF n'est pas relue.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::orient::Orientation;
use libheif_rs::{ColorSpace, DecodingOptions, HeifContext, ImageHandle, ItemId, LibHeif, RgbChroma};
use std::path::Path;
use std::sync::OnceLock;

/// Une seule initialisation de libheif (chargement des plugins de codec) pour tout le process.
fn lib() -> &'static LibHeif {
    static LIB: OnceLock<LibHeif> = OnceLock::new();
    LIB.get_or_init(LibHeif::new)
}

pub(super) fn decode(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    let data = std::fs::read(path)?;
    let ctx = HeifContext::read_from_bytes(&data).map_err(Error::decode)?;
    let handle = ctx.primary_image_handle().map_err(Error::decode)?;
    let (full_w, full_h) = (handle.width(), handle.height());

    if target == Target::Preview
        && let Some(mut frame) = embedded_thumbnail(&handle)?
    {
        // une thumbnail dont le ratio differe de l'image est tournee autrement : on ne s'en sert pas
        if ratio_matches(frame.w, frame.h, full_w, full_h) {
            frame.full_w = full_w;
            frame.full_h = full_h;
            return Ok(frame);
        }
    }

    let (rgba, w, h, has_alpha) = decode_handle(&handle)?;
    Plan::new(w, h, Orientation::Normal, target, max_full_bytes).finish(rgba, w, h, has_alpha, Orientation::Normal)
}

/// Memes proportions, a l'arrondi d'un pixel de la thumbnail pres.
fn ratio_matches(w: u32, h: u32, full_w: u32, full_h: u32) -> bool {
    let (a, b) = (w as u64 * full_h as u64, h as u64 * full_w as u64);
    a.abs_diff(b) <= full_w.max(full_h) as u64
}

/// Premiere thumbnail embarquee, decodee telle quelle (`full_w` et `full_h` a corriger par l'appelant).
fn embedded_thumbnail(handle: &ImageHandle) -> Result<Option<Frame>> {
    let mut ids: [ItemId; 1] = [0];
    if handle.thumbnail_ids(&mut ids) == 0 {
        return Ok(None);
    }
    let Ok(thumb) = handle.thumbnail(ids[0]) else {
        return Ok(None);
    };
    let Ok((rgba, w, h, _)) = decode_handle(&thumb) else {
        return Ok(None);
    };
    Ok(Some(Frame { rgba, w, h, full_w: w, full_h: h }))
}

/// Decode en RGBA 8 bits (les 10 bits sont ramenes a 8). Rend le buffer, ses dimensions et la presence d'alpha.
fn decode_handle(handle: &ImageHandle) -> Result<(Vec<u8>, u32, u32, bool)> {
    let mut options = DecodingOptions::new().ok_or_else(|| Error::Decode("libheif : options de decode".into()))?;
    options.set_convert_hdr_to_8bit(true);
    let image = lib().decode(handle, ColorSpace::Rgb(RgbChroma::Rgba), Some(options)).map_err(Error::decode)?;
    let plane = image.planes().interleaved.ok_or_else(|| Error::Decode("libheif : pas de plan RGBA".into()))?;
    let (w, h) = (plane.width, plane.height);
    let row = w as usize * 4;
    let mut rgba = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        rgba.extend_from_slice(&plane.data[y * plane.stride..y * plane.stride + row]);
    }
    Ok((rgba, w, h, handle.has_alpha_channel()))
}
