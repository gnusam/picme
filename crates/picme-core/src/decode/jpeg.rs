//! JPEG par libjpeg-turbo : DCT scaling n/8 a la decompression, orientation lue dans l'EXIF.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::orient::Orientation;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use turbojpeg::{Decompressor, Image, PixelFormat};

/// Dimension d'un cote reduit de n/8 (TJSCALED : arrondi par exces).
fn scaled(d: u32, n: u32) -> u32 {
    (d * n).div_ceil(8)
}

/// Dimensions de l'image entiere, orientation appliquee, lues dans le debut du fichier : l'en-tete SOF et l'EXIF
/// tiennent presque toujours dans les premiers 64 Kio ; sinon (gros APP2 avant le SOF) le fichier est relu en entier.
pub(super) fn probe(path: &Path) -> Option<(u32, u32)> {
    const HEAD: u64 = 64 << 10;
    let mut buf = Vec::new();
    File::open(path).ok()?.take(HEAD).read_to_end(&mut buf).ok()?;
    let mut dec = Decompressor::new().ok()?;
    // un en-tete tronque rend parfois Ok avec des dimensions nulles plutot qu'une erreur
    let read = |dec: &mut Decompressor, buf: &[u8]| dec.read_header(buf).ok().filter(|h| h.width > 0 && h.height > 0);
    let header = match read(&mut dec, &buf) {
        Some(h) => h,
        None if buf.len() as u64 == HEAD => {
            buf = std::fs::read(path).ok()?;
            read(&mut dec, &buf)?
        }
        None => return None,
    };
    let orient = Orientation::from_container(&mut Cursor::new(&buf));
    Some(orient.displayed_dims(header.width as u32, header.height as u32))
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
