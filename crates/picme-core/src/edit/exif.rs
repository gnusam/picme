//! Tag Orientation et dimensions dans les metadonnees, modifies sans toucher au reste : structure TIFF de l'EXIF
//! (APP1 d'un JPEG, chunk eXIf d'un PNG) et XMP d'un JPEG.
//!
//! Le patch se fait sur place quand le tag existe. Sinon le tag est ajoute en recopiant l'IFD0 a la fin de la
//! structure TIFF, l'en-tete pointant vers la copie : aucun autre octet ne bouge, les offsets (MakerNote compris)
//! restent justes. Pas de crate EXIF en ecriture : little_exif n'est pas dans le registry du conteneur, et
//! kamadak-exif ne fait que lire (il sert a relire le resultat dans les tests et les verifications).

const ORIENTATION: u16 = 0x0112;
const EXIF_IFD: u16 = 0x8769;
const PIXEL_X: u16 = 0xA002;
const PIXEL_Y: u16 = 0xA003;
const SHORT: u16 = 3;
const LONG: u16 = 4;
/// Longueur maximale d'un segment JPEG, marqueur exclu.
const MAX_SEGMENT: usize = 0xFFFF;
const EXIF_HDR: &[u8] = b"Exif\0\0";
const XMP_HDR: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// Ordre des octets d'une structure TIFF (vrai : big-endian).
fn order(d: &[u8]) -> Option<bool> {
    match d.get(..4)? {
        b"MM\0*" => Some(true),
        b"II*\0" => Some(false),
        _ => None,
    }
}

fn u16_at(d: &[u8], off: usize, be: bool) -> Option<u16> {
    let b: [u8; 2] = d.get(off..off.checked_add(2)?)?.try_into().ok()?;
    Some(if be { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
}

fn u32_at(d: &[u8], off: usize, be: bool) -> Option<u32> {
    let b: [u8; 4] = d.get(off..off.checked_add(4)?)?.try_into().ok()?;
    Some(if be { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
}

fn u16_bytes(v: u16, be: bool) -> [u8; 2] {
    if be { v.to_be_bytes() } else { v.to_le_bytes() }
}

fn u32_bytes(v: u32, be: bool) -> [u8; 4] {
    if be { v.to_be_bytes() } else { v.to_le_bytes() }
}

/// Position de l'entree `tag` (12 octets) dans l'IFD qui commence a `ifd`.
fn find_entry(d: &[u8], be: bool, ifd: usize, tag: u16) -> Option<usize> {
    let n = usize::from(u16_at(d, ifd, be)?);
    (0..n).map(|i| ifd + 2 + 12 * i).take_while(|&e| e + 12 <= d.len()).find(|&e| u16_at(d, e, be) == Some(tag))
}

/// Ecrit `v` dans l'entree `e` (SHORT ou LONG, un seul element, valeur logee dans l'entree). Faux pour un autre type.
fn put_value(d: &mut [u8], be: bool, e: usize, v: u32) -> bool {
    match (u16_at(d, e + 2, be), u32_at(d, e + 4, be)) {
        (Some(SHORT), Some(1)) if v <= u32::from(u16::MAX) => {
            // un SHORT tient dans les deux premiers octets du champ valeur, dans les deux ordres
            d[e + 8..e + 10].copy_from_slice(&u16_bytes(v as u16, be));
            true
        }
        (Some(LONG), Some(1)) => {
            d[e + 8..e + 12].copy_from_slice(&u32_bytes(v, be));
            true
        }
        _ => false,
    }
}

/// Orientation de l'IFD0 mise a `v` sur place. Faux si le tag est absent ou d'un type inattendu.
pub(super) fn set_orientation(tiff: &mut [u8], v: u16) -> bool {
    let Some(be) = order(tiff) else { return false };
    let Some(ifd) = u32_at(tiff, 4, be) else { return false };
    match find_entry(tiff, be, ifd as usize, ORIENTATION) {
        Some(e) => put_value(tiff, be, e, v.into()),
        None => false,
    }
}

/// Entree Orientation (SHORT, un element) de valeur `v`.
fn orientation_entry(v: u16, be: bool) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&u16_bytes(ORIENTATION, be));
    e[2..4].copy_from_slice(&u16_bytes(SHORT, be));
    e[4..8].copy_from_slice(&u32_bytes(1, be));
    e[8..10].copy_from_slice(&u16_bytes(v, be));
    e
}

/// Copie de `tiff` avec un tag Orientation ajoute a l'IFD0 (qui n'en a pas) : l'IFD0 est recopie a la fin avec
/// l'entree inseree a sa place (tags croissants) et l'en-tete pointe vers la copie. `None` si la structure est
/// illisible ou a deja le tag.
pub(super) fn with_orientation(tiff: &[u8], v: u16) -> Option<Vec<u8>> {
    let be = order(tiff)?;
    let ifd = u32_at(tiff, 4, be)? as usize;
    let n = usize::from(u16_at(tiff, ifd, be)?);
    let entries = tiff.get(ifd + 2..ifd + 2 + 12 * n)?;
    let next = tiff.get(ifd + 2 + 12 * n..ifd + 6 + 12 * n)?;
    let mut out = tiff.to_vec();
    // un IFD commence a un offset pair (TIFF 6.0)
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let new_ifd = u32::try_from(out.len()).ok()?;
    out.extend_from_slice(&u16_bytes(u16::try_from(n + 1).ok()?, be));
    let mut placed = false;
    for e in entries.as_chunks::<12>().0 {
        let tag = u16_at(e, 0, be)?;
        if tag == ORIENTATION {
            return None;
        }
        if !placed && tag > ORIENTATION {
            out.extend_from_slice(&orientation_entry(v, be));
            placed = true;
        }
        out.extend_from_slice(e);
    }
    if !placed {
        out.extend_from_slice(&orientation_entry(v, be));
    }
    out.extend_from_slice(next);
    out[4..8].copy_from_slice(&u32_bytes(new_ifd, be));
    Some(out)
}

/// Structure TIFF minimale : en-tete big-endian et un IFD0 avec Orientation et YCbCrPositioning (centre), seul tag
/// obligatoire de l'IFD0 d'un JPEG que `exiftool -validate` reclame en son absence.
pub(super) fn minimal(v: u16) -> Vec<u8> {
    let mut d = b"MM\0*\0\0\0\x08\0\x02".to_vec();
    d.extend_from_slice(&orientation_entry(v, true));
    d.extend_from_slice(&[0x02, 0x13, 0, SHORT as u8, 0, 0, 0, 1, 0, 1, 0, 0]);
    d.extend_from_slice(&[0; 4]);
    d
}

/// PixelXDimension et PixelYDimension de l'IFD EXIF mis a jour sur place, s'ils existent (rotation qui echange les
/// axes, crop). libjpeg-turbo ne les ajuste que si l'APP1 EXIF est le premier segment conserve.
pub(super) fn set_pixel_dims(tiff: &mut [u8], w: u32, h: u32) {
    let Some(be) = order(tiff) else { return };
    let Some(ifd0) = u32_at(tiff, 4, be) else { return };
    let Some(sub) = find_entry(tiff, be, ifd0 as usize, EXIF_IFD).and_then(|e| u32_at(tiff, e + 8, be)) else { return };
    for (tag, v) in [(PIXEL_X, w), (PIXEL_Y, h)] {
        if let Some(e) = find_entry(tiff, be, sub as usize, tag) {
            put_value(tiff, be, e, v);
        }
    }
}

/// Segments d'un JPEG avant les donnees d'image : (marqueur, debut du segment, fin).
fn segments(jpeg: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut v = Vec::new();
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return v;
    }
    let mut i = 2;
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let m = jpeg[i + 1];
        if m == 0xFF {
            // octet de bourrage avant un marqueur
            i += 1;
            continue;
        }
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        v.push((m, i, i + 2 + len));
        i += 2 + len;
    }
    v
}

/// APP1 EXIF : (debut du segment, debut de la structure TIFF, fin du segment).
fn jpeg_exif(jpeg: &[u8]) -> Option<(usize, usize, usize)> {
    segments(jpeg).into_iter().find_map(|(m, s, e)| (m == 0xE1 && jpeg[s + 4..e].starts_with(EXIF_HDR)).then_some((s, s + 4 + EXIF_HDR.len(), e)))
}

/// Segment APP1 EXIF complet pour la structure `tiff`. `None` s'il depasse la taille d'un segment.
fn app1(tiff: &[u8]) -> Option<Vec<u8>> {
    let len = u16::try_from(2 + EXIF_HDR.len() + tiff.len()).ok().filter(|&l| usize::from(l) <= MAX_SEGMENT)?;
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(EXIF_HDR);
    seg.extend_from_slice(tiff);
    Some(seg)
}

/// Tag Orientation du JPEG mis a `v` : patch sur place s'il existe, sinon ajout (APP1 cree apres le SOI et l'APP0
/// JFIF, ou IFD0 recopie). L'XMP suit. `None` si c'est impossible (EXIF illisible, APP1 trop grand).
pub(super) fn jpeg_set_orientation(mut jpeg: Vec<u8>, v: u16) -> Option<Vec<u8>> {
    match jpeg_exif(&jpeg) {
        Some((_, t, e)) if set_orientation(&mut jpeg[t..e], v) => {}
        // 1 est la valeur par defaut : rien a ajouter
        Some(_) if v == 1 => {}
        Some((s, t, e)) => {
            let seg = app1(&with_orientation(&jpeg[t..e], v)?)?;
            jpeg.splice(s..e, seg);
        }
        None if v == 1 => {}
        None => {
            let seg = app1(&minimal(v))?;
            // JFIF exige d'etre le premier segment : l'EXIF vient juste apres
            let at = match segments(&jpeg).first() {
                Some(&(0xE0, s, e)) if jpeg[s + 4..e].starts_with(b"JFIF\0") => e,
                _ => 2,
            };
            jpeg.splice(at..at, seg);
        }
    }
    set_xmp_orientation(&mut jpeg, v);
    Some(jpeg)
}

/// Dimensions EXIF du JPEG mises a jour sur place (voir [`set_pixel_dims`]).
pub(super) fn jpeg_set_pixel_dims(jpeg: &mut [u8], w: u32, h: u32) {
    if let Some((_, t, e)) = jpeg_exif(jpeg) {
        set_pixel_dims(&mut jpeg[t..e], w, h);
    }
}

/// `tiff:Orientation` de l'XMP (attribut ou element) mis a `v` sur place : un seul chiffre, la longueur du segment
/// ne change pas. Sans XMP ou sans ce champ, rien.
fn set_xmp_orientation(jpeg: &mut [u8], v: u16) {
    let digit = b'0' + v as u8;
    for (m, s, e) in segments(jpeg) {
        let seg = &mut jpeg[s + 4..e];
        if m != 0xE1 || !seg.starts_with(XMP_HDR) {
            continue;
        }
        for pat in [&b"tiff:Orientation=\""[..], b"tiff:Orientation='", b"tiff:Orientation>"] {
            let mut from = 0;
            while let Some(p) = seg[from..].windows(pat.len()).position(|w| w == pat) {
                let d = from + p + pat.len();
                if seg.get(d).is_some_and(u8::is_ascii_digit) && !seg.get(d + 1).is_some_and(u8::is_ascii_digit) {
                    seg[d] = digit;
                }
                from = d;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orient::Orientation;
    use std::io::Cursor;

    fn orientation_of(jpeg: &[u8]) -> Orientation {
        Orientation::from_container(&mut Cursor::new(jpeg))
    }

    /// TIFF little-endian sans Orientation : IFD0 a deux tags (ImageWidth 0x0100, Make 0x010F) et un IFD EXIF
    /// (PixelX/YDimension en LONG), la chaine de Make hors de l'IFD pour verifier que les offsets restent justes.
    fn tiff_without_orientation() -> Vec<u8> {
        let mut d = b"II*\0\x08\0\0\0".to_vec();
        // IFD0 a 8 : 3 entrees, puis next = 0
        d.extend_from_slice(&3u16.to_le_bytes());
        let entry = |tag: u16, typ: u16, count: u32, val: u32| {
            let mut e = Vec::new();
            e.extend_from_slice(&tag.to_le_bytes());
            e.extend_from_slice(&typ.to_le_bytes());
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&val.to_le_bytes());
            e
        };
        let ifd0_end = 8 + 2 + 3 * 12 + 4;
        let make_at = ifd0_end;
        let exif_at = make_at + 6;
        d.extend(entry(0x0100, LONG, 1, 640));
        d.extend(entry(0x010F, 2, 6, make_at as u32));
        d.extend(entry(EXIF_IFD, LONG, 1, exif_at as u32));
        d.extend_from_slice(&[0; 4]);
        d.extend_from_slice(b"Canon\0");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend(entry(PIXEL_X, LONG, 1, 640));
        d.extend(entry(PIXEL_Y, SHORT, 1, 480));
        d.extend_from_slice(&[0; 4]);
        d
    }

    fn read_exif(tiff: &[u8]) -> exif::Exif {
        exif::Reader::new().read_raw(tiff.to_vec()).unwrap()
    }

    fn uint(e: &exif::Exif, tag: exif::Tag, ifd: exif::In) -> Option<u32> {
        e.get_field(tag, ifd).and_then(|f| f.value.get_uint(0))
    }

    #[test]
    fn inserted_orientation_keeps_other_tags() {
        let t = tiff_without_orientation();
        assert!(!set_orientation(&mut t.clone(), 6), "pas de tag : pas de patch");
        let out = with_orientation(&t, 6).unwrap();
        let mut expect = t.clone();
        expect[4..8].copy_from_slice(&out[4..8]);
        assert_eq!(&out[..t.len()], &expect[..], "seul l'offset d'IFD0 change dans l'existant");
        let e = read_exif(&out);
        assert_eq!(uint(&e, exif::Tag::Orientation, exif::In::PRIMARY), Some(6));
        assert_eq!(uint(&e, exif::Tag::ImageWidth, exif::In::PRIMARY), Some(640));
        assert_eq!(e.get_field(exif::Tag::Make, exif::In::PRIMARY).unwrap().display_value().to_string(), "\"Canon\"");
        assert_eq!(uint(&e, exif::Tag::PixelXDimension, exif::In::PRIMARY), Some(640));
        assert!(with_orientation(&out, 3).is_none(), "tag deja present : ajout refuse");
        let mut out = out;
        assert!(set_orientation(&mut out, 3));
        assert_eq!(uint(&read_exif(&out), exif::Tag::Orientation, exif::In::PRIMARY), Some(3));
    }

    #[test]
    fn pixel_dims_patched_short_and_long() {
        let mut t = tiff_without_orientation();
        set_pixel_dims(&mut t, 480, 640);
        let e = read_exif(&t);
        assert_eq!(uint(&e, exif::Tag::PixelXDimension, exif::In::PRIMARY), Some(480));
        assert_eq!(uint(&e, exif::Tag::PixelYDimension, exif::In::PRIMARY), Some(640));
    }

    #[test]
    fn jpeg_without_exif_gets_app1_after_jfif() {
        let src = std::fs::read(crate::fixtures_dir().join("a.jpg")).unwrap();
        assert!(jpeg_exif(&src).is_none());
        let out = jpeg_set_orientation(src.clone(), 8).unwrap();
        assert_eq!(orientation_of(&out), Orientation::Rotate270);
        let segs = segments(&out);
        assert_eq!(segs[0].0, 0xE0, "JFIF reste premier");
        assert_eq!(segs[1].0, 0xE1, "EXIF juste apres");
        assert_eq!(&out[segs[1].2..], &src[segments(&src)[0].2..], "le reste du fichier est intact");
        // valeur par defaut sans EXIF : fichier inchange
        assert_eq!(jpeg_set_orientation(src.clone(), 1).unwrap(), src);
    }

    #[test]
    fn jpeg_orientation_patched_in_place() {
        let src = std::fs::read(crate::fixtures_dir().join("c_orient6.jpg")).unwrap();
        assert_eq!(orientation_of(&src), Orientation::Rotate90);
        let out = jpeg_set_orientation(src.clone(), 1).unwrap();
        assert_eq!(out.len(), src.len());
        assert_eq!(orientation_of(&out), Orientation::Normal);
        assert_eq!(src.iter().zip(&out).filter(|(a, b)| a != b).count(), 1, "un seul octet change");
    }

    #[test]
    fn xmp_orientation_follows() {
        let mut jpeg = vec![0xFF, 0xD8];
        let xmp = [XMP_HDR, b"<x tiff:Orientation=\"6\"/><tiff:Orientation>6</tiff:Orientation>"].concat();
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&u16::try_from(xmp.len() + 2).unwrap().to_be_bytes());
        jpeg.extend_from_slice(&xmp);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        set_xmp_orientation(&mut jpeg, 1);
        let s = String::from_utf8_lossy(&jpeg);
        assert!(s.contains("tiff:Orientation=\"1\"") && s.contains("<tiff:Orientation>1<"), "{s}");
    }
}
