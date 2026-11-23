//! JPEG par libjpeg-turbo : DCT scaling n/8 a la decompression, orientation lue dans l'EXIF.

use super::{Frame, Plan, Target};
use crate::error::{Error, Result};
use crate::orient::Orientation;
use std::ffi::{CStr, c_int, c_ulong};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use turbojpeg::{Decompressor, PixelFormat, raw};

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

/// `tjDecompress2` sans le wrapper de turbojpeg, qui n'expose pas le code d'erreur : TurboJPEG 2.1 rend -1 sur un
/// simple warning de libjpeg (`Premature end of JPEG file`, `Corrupt JPEG data`) alors que le buffer est entierement
/// ecrit, le reste de l'image etant gris. `Ok(true)` : decode partiel (warning) accepte par `lenient` ; une erreur
/// fatale, ou un warning sans `lenient`, est une `Err`.
fn decompress(data: &[u8], out: &mut [u8], w: usize, h: usize, lenient: bool) -> Result<bool> {
    struct Handle(raw::tjhandle);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { raw::tjDestroy(self.0) };
        }
    }
    assert!(out.len() >= w * h * 4);
    let handle = unsafe { raw::tjInitDecompress() };
    if handle.is_null() {
        return Err(Error::decode("TurboJPEG returned null pointer"));
    }
    let handle = Handle(handle);
    // SAFETY : `out` tient w x h pixels RGBA (assert ci-dessus), `data` vit pendant l'appel
    let res = unsafe {
        raw::tjDecompress2(handle.0, data.as_ptr(), data.len() as c_ulong, out.as_mut_ptr(), w as c_int, w as c_int * 4, h as c_int, PixelFormat::RGBA as c_int, 0)
    };
    if res == 0 {
        return Ok(false);
    }
    let msg = unsafe { CStr::from_ptr(raw::tjGetErrorStr2(handle.0)) }.to_string_lossy().into_owned();
    let warning = unsafe { raw::tjGetErrorCode(handle.0) } == raw::TJERR_TJERR_WARNING as c_int;
    if warning && lenient { Ok(true) } else { Err(Error::decode(format!("TurboJPEG error: {msg}"))) }
}

pub(super) fn decode(path: &Path, target: Target, max_full_bytes: usize, lenient: bool) -> Result<Frame> {
    let data = std::fs::read(path)?;
    let mut dec = Decompressor::new().map_err(Error::decode)?;
    let header = dec.read_header(&data).map_err(Error::decode)?;
    let (sw, sh) = (header.width as u32, header.height as u32);
    // un en-tete tronque rend parfois Ok avec des dimensions nulles : rien a afficher, meme en mode partiel
    if sw == 0 || sh == 0 {
        return Err(Error::decode("JPEG sans dimensions (en-tete tronque)"));
    }
    let orient = Orientation::from_container(&mut Cursor::new(&data));
    let plan = Plan::new(sw, sh, orient, target, max_full_bytes);

    // Plus petit facteur n/8 dont le resultat couvre la cible (jamais d'agrandissement ensuite). tjDecompress2
    // choisit lui-meme le plus grand facteur qui tient dans les dimensions demandees : on lui donne celles de n/8.
    let n = (1..=8).find(|&n| scaled(sw, n) >= plan.stored_w && scaled(sh, n) >= plan.stored_h).unwrap_or(8);
    let (dw, dh) = (scaled(sw, n), scaled(sh, n));
    let mut rgba = vec![0u8; dw as usize * dh as usize * 4];
    let partial = decompress(&data, &mut rgba, dw as usize, dh as usize, lenient)?;
    Ok(Frame { partial, ..plan.finish(rgba, dw, dh, false, orient)? })
}
