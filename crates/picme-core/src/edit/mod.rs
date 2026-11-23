//! Modifications ecrites sur disque : ecriture atomique, rotation et crop sans perte quand le format le permet.
//! Appele depuis le thread unique des operations fichier de l'UI, jamais du main thread.

pub mod atomic;
pub mod crop;
mod exif;
mod png;
pub mod rotate;

use crate::error::{Error, Result};
use crate::orient::Orientation;
use std::io::Cursor;
use turbojpeg::{Colorspace, Decompressor, Image, PixelFormat};

/// Verifie un JPEG avant le rename : dimensions stockees, orientation EXIF, et decodage de tout le flux a 1/8
/// (l'entropie est lue en entier meme a cette echelle).
fn check_jpeg(data: &[u8], w: u32, h: u32, orient: Orientation) -> Result<()> {
    let mut dec = Decompressor::new().map_err(Error::decode)?;
    let header = dec.read_header(data).map_err(Error::decode)?;
    if (header.width, header.height) != (w as usize, h as usize) {
        return Err(Error::Invalid(format!("JPEG ecrit en {}x{}, attendu {w}x{h}", header.width, header.height)));
    }
    let got = Orientation::from_container(&mut Cursor::new(data));
    if got != orient {
        return Err(Error::Invalid(format!("orientation ecrite {got:?}, attendue {orient:?}")));
    }
    let (format, n) = match header.colorspace {
        Colorspace::CMYK | Colorspace::YCCK => (PixelFormat::CMYK, 4),
        _ => (PixelFormat::RGB, 3),
    };
    let (dw, dh) = (header.width.div_ceil(8), header.height.div_ceil(8));
    let mut px = vec![0u8; dw * dh * n];
    dec.decompress(data, Image { pixels: &mut px[..], width: dw, pitch: dw * n, height: dh, format }).map_err(Error::decode)
}
