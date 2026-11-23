//! JPEG par libjpeg-turbo : DCT scaling n/8 a la decompression, orientation lue dans l'EXIF.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::orient::Orientation;
use std::io::Cursor;
use std::path::Path;
use turbojpeg::{Decompressor, Image, PixelFormat};

/// Dimension d'un cote reduit de n/8 (TJSCALED : arrondi par exces).
fn scaled(d: u32, n: u32) -> u32 {
    (d * n).div_ceil(8)
}

pub(super) fn decode(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    let data = std::fs::read(path)?;
    let mut dec = Decompressor::new().map_err(Error::decode)?;
    let header = dec.read_header(&data).map_err(Error::decode)?;
    let (sw, sh) = (header.width as u32, header.height as u32);
    let orient = Orientation::from_container(&mut Cursor::new(&data));
    let plan = Plan::new(sw, sh, orient, target, max_full_bytes);

    // Plus petit facteur n/8 dont le resultat couvre la cible (jamais d'agrandissement ensuite). tjDecompress2
    // choisit lui-meme le plus grand facteur qui tient dans les dimensions demandees : on lui donne celles de n/8.
    let n = (1..=8).find(|&n| scaled(sw, n) >= plan.stored_w && scaled(sh, n) >= plan.stored_h).unwrap_or(8);
    let (dw, dh) = (scaled(sw, n), scaled(sh, n));
    let mut rgba = vec![0u8; dw as usize * dh as usize * 4];
    let out = Image { pixels: &mut rgba[..], width: dw as usize, pitch: dw as usize * 4, height: dh as usize, format: PixelFormat::RGBA };
    dec.decompress(&data, out).map_err(Error::decode)?;
    plan.finish(rgba, dw, dh, false, orient)
}
