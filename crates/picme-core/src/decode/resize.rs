//! Reduction RGBA8 par fast_image_resize.

use crate::error::{Error, Result};
use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

/// Redimensionne un buffer RGBA8 de `w` x `h` en `nw` x `nh` (rendu tel quel si les dimensions sont deja bonnes).
/// `has_alpha` faux : alpha ignore, ce qui evite la multiplication et la division par alpha autour du filtre.
pub(super) fn resize_rgba(buf: Vec<u8>, w: u32, h: u32, nw: u32, nh: u32, has_alpha: bool) -> Result<Vec<u8>> {
    if (w, h) == (nw, nh) {
        return Ok(buf);
    }
    let src = Image::from_vec_u8(w, h, buf, PixelType::U8x4).map_err(Error::decode)?;
    let mut dst = Image::new(nw, nh, PixelType::U8x4);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)).use_alpha(has_alpha);
    Resizer::new().resize(&src, &mut dst, &options).map_err(Error::decode)?;
    Ok(dst.into_vec())
}
