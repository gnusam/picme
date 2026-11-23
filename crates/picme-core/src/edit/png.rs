//! PNG reecrit sans perte : pixels transformes (rotation, crop) au bit pres, dans la profondeur et le type de couleur
//! d'origine, et chunks d'origine conserves.
//!
//! L'encodeur ne fournit que l'IHDR et les IDAT ; les autres chunks sont recopies du fichier d'origine, dans leur
//! ordre (palette, transparence, profil ICC, gamma, textes, eXIf...). eXIf : Orientation mise a la valeur voulue.
//! pHYs : densites echangees quand les axes s'echangent. Un chunk inconnu marque "unsafe to copy" (4e lettre
//! majuscule) est ecarte, comme le demande la norme quand les pixels changent. Un APNG est refuse.

use super::exif;
use crate::error::{Error, Result};
use crate::orient::{Orientation, Rect};
use std::io::Cursor;

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Chunks connus dont le sens ne depend pas de la disposition des pixels, gardes meme s'ils sont "unsafe to copy".
const KNOWN: &[&[u8; 4]] = &[
    b"PLTE", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB", b"bKGD", b"hIST", b"cICP", b"mDCV", b"cLLI", b"tEXt", b"zTXt", b"iTXt",
    b"tIME", b"pHYs", b"sPLT", b"eXIf",
];

struct Chunk {
    kind: [u8; 4],
    data: Vec<u8>,
}

/// Chunks d'un fichier PNG, sans verifier les CRC (le decodeur l'a deja fait).
fn chunks(data: &[u8]) -> Result<Vec<Chunk>> {
    let bad = || Error::Decode("PNG : structure de chunks illisible".into());
    let mut rest = data.strip_prefix(SIGNATURE).ok_or_else(bad)?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let len = u32::from_be_bytes(rest.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let kind: [u8; 4] = rest.get(4..8).ok_or_else(bad)?.try_into().unwrap();
        let body = rest.get(8..8 + len).ok_or_else(bad)?;
        out.push(Chunk { kind, data: body.to_vec() });
        rest = rest.get(12 + len..).ok_or_else(bad)?;
        if &kind == b"IEND" {
            break;
        }
    }
    Ok(out)
}

fn put_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

/// Image PNG decodee sans conversion : un pixel par `px` octets (profondeurs 1, 2 et 4 depaquetees a un octet).
pub(super) struct Raw {
    pub(super) w: u32,
    pub(super) h: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    px: usize,
    pixels: Vec<u8>,
    palette: Option<Vec<u8>>,
    chunks: Vec<Chunk>,
}

pub(super) fn decode(data: &[u8]) -> Result<Raw> {
    let mut dec = png::Decoder::new(Cursor::new(data));
    dec.set_transformations(png::Transformations::IDENTITY);
    let mut reader = dec.read_info().map_err(Error::decode)?;
    let info = reader.info();
    if info.animation_control.is_some() {
        return Err(Error::Unsupported("PNG anime (APNG)".into()));
    }
    let (color, depth) = (info.color_type, info.bit_depth);
    let palette = info.palette.as_ref().map(|p| p.to_vec());
    let size = reader.output_buffer_size().ok_or_else(|| Error::Invalid("PNG trop grand".into()))?;
    let mut buf = vec![0u8; size];
    let out = reader.next_frame(&mut buf).map_err(Error::decode)?;
    let (w, h) = (out.width, out.height);
    let bits = depth as usize;
    let (px, pixels) = if bits < 8 {
        (1, unpack(&buf, out.line_size, w as usize, h as usize, bits))
    } else {
        buf.truncate(out.line_size * h as usize);
        (color.samples() * bits / 8, buf)
    };
    Ok(Raw { w, h, color, depth, px, pixels, palette, chunks: chunks(data)? })
}

/// Lignes paquetees a `bits` bits par pixel (un seul canal) -> un octet par pixel.
fn unpack(buf: &[u8], line: usize, w: usize, h: usize, bits: usize) -> Vec<u8> {
    let mask = (1u8 << bits) - 1;
    let mut out = Vec::with_capacity(w * h);
    for row in buf.chunks(line).take(h) {
        out.extend((0..w).map(|i| (row[i * bits / 8] >> (8 - bits - (i * bits) % 8)) & mask));
    }
    out
}

/// Inverse de [`unpack`] : lignes commencant chacune sur un octet, comme le veut le PNG.
fn pack(pixels: &[u8], w: usize, bits: usize) -> Vec<u8> {
    let line = (w * bits).div_ceil(8);
    let mut out = vec![0u8; line * (pixels.len() / w)];
    for (row, dst) in pixels.chunks(w).zip(out.chunks_mut(line)) {
        for (i, &v) in row.iter().enumerate() {
            dst[i * bits / 8] |= v << (8 - bits - (i * bits) % 8);
        }
    }
    out
}

impl Raw {
    /// Pixels transformes par `t` (stocke -> affiche).
    pub(super) fn transformed(mut self, t: Orientation) -> Raw {
        let (buf, w, h) = (std::mem::take(&mut self.pixels), self.w, self.h);
        let (pixels, w, h) = match self.px {
            1 => t.apply_px::<1>(buf, w, h),
            2 => t.apply_px::<2>(buf, w, h),
            3 => t.apply_px::<3>(buf, w, h),
            4 => t.apply_px::<4>(buf, w, h),
            6 => t.apply_px::<6>(buf, w, h),
            _ => t.apply_px::<8>(buf, w, h),
        };
        Raw { w, h, pixels, ..self }
    }

    /// Pixels du rectangle `r` (en pixels stockes).
    pub(super) fn cropped(mut self, r: Rect) -> Raw {
        let (line, px) = (self.w as usize * self.px, self.px);
        let mut out = Vec::with_capacity(r.w as usize * r.h as usize * px);
        for row in self.pixels.chunks(line).skip(r.y as usize).take(r.h as usize) {
            out.extend_from_slice(&row[r.x as usize * px..(r.x + r.w) as usize * px]);
        }
        self.pixels = out;
        Raw { w: r.w, h: r.h, ..self }
    }

    /// Fichier PNG : IHDR et IDAT de l'encodeur, autres chunks d'origine. `orient` est la valeur du tag Orientation
    /// a ecrire dans l'eXIf (ajoute s'il manque et que la valeur n'est pas 1) ; `swapped` : les axes ont tourne.
    pub(super) fn encode(&self, orient: Orientation, swapped: bool) -> Result<Vec<u8>> {
        let bits = self.depth as usize;
        let packed;
        let data = if bits < 8 {
            packed = pack(&self.pixels, self.w as usize, bits);
            &packed[..]
        } else {
            &self.pixels[..]
        };
        let mut encoded = Vec::new();
        let mut enc = png::Encoder::new(&mut encoded, self.w, self.h);
        enc.set_color(self.color);
        enc.set_depth(self.depth);
        // l'encodeur exige la palette d'une image indexee ; c'est le PLTE d'origine qui est recopie
        if let Some(p) = &self.palette {
            enc.set_palette(p.clone());
        }
        enc.set_compression(png::Compression::High);
        let mut writer = enc.write_header().map_err(Error::decode)?;
        writer.write_image_data(data).map_err(Error::decode)?;
        writer.finish().map_err(Error::decode)?;
        let fresh = chunks(&encoded)?;

        let mut out = SIGNATURE.to_vec();
        let ihdr = fresh.iter().find(|c| &c.kind == b"IHDR").ok_or_else(|| Error::Invalid("encodeur PNG sans IHDR".into()))?;
        put_chunk(&mut out, &ihdr.kind, &ihdr.data);
        let mut idat_done = false;
        let mut exif_done = false;
        for c in &self.chunks {
            match &c.kind {
                b"IHDR" | b"IEND" | b"acTL" | b"fcTL" | b"fdAT" => {}
                b"IDAT" if idat_done => {}
                b"IDAT" => {
                    // avant les IDAT : un eXIf a creer doit preceder l'image pour etre lu par tous
                    if !exif_done && orient != Orientation::Normal && !self.chunks.iter().any(|c| &c.kind == b"eXIf") {
                        put_chunk(&mut out, b"eXIf", &exif::minimal(orient.exif_value()));
                        exif_done = true;
                    }
                    for d in fresh.iter().filter(|c| &c.kind == b"IDAT") {
                        put_chunk(&mut out, b"IDAT", &d.data);
                    }
                    idat_done = true;
                }
                b"eXIf" => {
                    let mut d = c.data.clone();
                    let v = orient.exif_value();
                    if !exif::set_orientation(&mut d, v)
                        && v != 1
                        && let Some(with) = exif::with_orientation(&d, v)
                    {
                        d = with;
                    }
                    put_chunk(&mut out, b"eXIf", &d);
                    exif_done = true;
                }
                b"pHYs" if swapped && c.data.len() == 9 => {
                    let mut d = c.data.clone();
                    d[..8].rotate_left(4);
                    put_chunk(&mut out, b"pHYs", &d);
                }
                k if KNOWN.contains(&k) || k[3] & 0x20 != 0 => put_chunk(&mut out, k, &c.data),
                _ => {}
            }
        }
        put_chunk(&mut out, b"IEND", &[]);
        Ok(out)
    }

    /// Verifie un PNG ecrit : memes pixels, meme format que `self`, orientation attendue.
    pub(super) fn check(&self, data: &[u8], orient: Orientation) -> Result<()> {
        let back = decode(data)?;
        if (back.w, back.h, back.color, back.depth) != (self.w, self.h, self.color, self.depth) || back.pixels != self.pixels {
            return Err(Error::Invalid("PNG relu different de l'image ecrite".into()));
        }
        let got = Orientation::from_container(&mut Cursor::new(data));
        if got != orient {
            return Err(Error::Invalid(format!("orientation ecrite {got:?}, attendue {orient:?}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_is_the_inverse_of_unpack() {
        for bits in [1usize, 2, 4] {
            let w = 13;
            let pixels: Vec<u8> = (0..w * 3).map(|i| (i * 7 % (1 << bits)) as u8).collect();
            let packed = pack(&pixels, w, bits);
            assert_eq!(unpack(&packed, (w * bits).div_ceil(8), w, 3, bits), pixels, "{bits} bits");
        }
    }
}
