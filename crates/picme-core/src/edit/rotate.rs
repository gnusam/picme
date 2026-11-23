//! Rotation enregistree (R, Shift+R). JPEG : transformation DCT sans re-encodage (tjTransform PERFECT, tous les
//! marqueurs copies), puis tag Orientation remis a 1. Si les dimensions ne sont pas multiples du MCU, les pixels ne
//! bougent pas et la rotation passe dans le tag Orientation. PNG : re-encode sans perte. Les autres formats sont
//! refuses (`Error::Unsupported`) : rotation de vue seule cote UI. Jamais de TRIM (il rognerait les bords).
//!
//! La rotation demandee se compose avec l'orientation EXIF existante : la nouvelle image affichee est l'ancienne
//! tournee, quel que soit le tag d'origine. Limite connue : la miniature EXIF embarquee n'est pas tournee.

use super::{atomic, check_jpeg, exif, png};
use crate::error::{Error, Result};
use crate::media::{self, MediaKind};
use crate::orient::Orientation;
use std::io::Cursor;
use std::path::Path;
use turbojpeg::{Decompressor, Subsamp, Transform, TransformOp, Transformer};

/// Comment la rotation a ete enregistree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    /// Pixels transformes sans perte, tag Orientation a 1.
    Pixels,
    /// Pixels intacts, rotation ecrite dans le tag Orientation (JPEG aux dimensions non multiples du MCU).
    Tag,
}

/// Enregistre la rotation `turn` (`Rotate90` pour R, `Rotate270` pour Shift+R ; toute orientation est acceptee)
/// de l'image `path`. Atomique : en cas d'erreur l'original est intact. Si un autre programme reecrit ou remplace le
/// fichier entre la lecture et le rename, la rotation est abandonnee (`Error::Invalid`) et sa version est conservee.
pub fn rotate(path: &Path, turn: Orientation) -> Result<Saved> {
    let (data, seen) = atomic::read(path)?;
    rotate_data(path, data, &seen, turn)
}

/// [`rotate`] sur le contenu deja lu (`seen` : son identite, rendue par [`atomic::read`]).
fn rotate_data(path: &Path, data: Vec<u8>, seen: &atomic::Seen, turn: Orientation) -> Result<Saved> {
    match media::sniff(&data) {
        Some(MediaKind::Jpeg) => rotate_jpeg(path, data, seen, turn),
        Some(MediaKind::Png) => rotate_png(path, &data, seen, turn),
        Some(kind) => Err(Error::Unsupported(format!("rotation non enregistrable en {kind:?}"))),
        None => Err(Error::Unsupported("format inconnu".into())),
    }
}

/// Operation libjpeg qui applique l'orientation `t` aux pixels stockes.
fn op_of(t: Orientation) -> TransformOp {
    match t {
        Orientation::Normal => TransformOp::None,
        Orientation::MirrorH => TransformOp::Hflip,
        Orientation::Rotate180 => TransformOp::Rot180,
        Orientation::MirrorV => TransformOp::Vflip,
        Orientation::Transpose => TransformOp::Transpose,
        Orientation::Rotate90 => TransformOp::Rot90,
        Orientation::Transverse => TransformOp::Transverse,
        Orientation::Rotate270 => TransformOp::Rot270,
    }
}

/// Vrai si `t` se fait sans perte sur une image `w` x `h` : un bord qui devient haut ou gauche doit tomber sur une
/// frontiere de MCU (meme regle que jtransform_perfect_transform de libjpeg-turbo).
fn perfect_possible(t: Orientation, w: u32, h: u32, subsamp: Subsamp) -> bool {
    let (mw, mh) = subsamp.mcu_size();
    let (wx, hx) = ((w as usize).is_multiple_of(mw), (h as usize).is_multiple_of(mh));
    match t {
        Orientation::Normal | Orientation::Transpose => true,
        Orientation::MirrorH | Orientation::Rotate270 => wx,
        Orientation::MirrorV | Orientation::Rotate90 => hx,
        Orientation::Rotate180 | Orientation::Transverse => wx && hx,
    }
}

fn rotate_jpeg(path: &Path, data: Vec<u8>, seen: &atomic::Seen, turn: Orientation) -> Result<Saved> {
    let header = Decompressor::new().map_err(Error::decode)?.read_header(&data).map_err(Error::decode)?;
    let (w, h) = (header.width as u32, header.height as u32);
    let t = Orientation::from_container(&mut Cursor::new(&data)).compose(turn);
    if t == Orientation::Normal || !perfect_possible(t, w, h, header.subsamp) {
        // pixels intacts : la rotation passe dans le tag (et le tag revient a 1 si elle annule l'orientation)
        let out = exif::jpeg_set_orientation(data, t.exif_value()).ok_or_else(|| Error::Unsupported("EXIF illisible ou trop grand pour y ecrire l'orientation".into()))?;
        atomic::replace(path, seen, &out, |b| check_jpeg(b, w, h, t))?;
        return Ok(if t == Orientation::Normal { Saved::Pixels } else { Saved::Tag });
    }
    let tr = Transform { op: op_of(t), perfect: true, ..Transform::default() };
    let out = Transformer::new().map_err(Error::decode)?.transform_to_vec(&tr, &data).map_err(Error::decode)?;
    let (dw, dh) = t.displayed_dims(w, h);
    let mut out = exif::jpeg_set_orientation(out, 1).ok_or_else(|| Error::Invalid("tag Orientation impossible a remettre a 1".into()))?;
    exif::jpeg_set_pixel_dims(&mut out, dw, dh);
    atomic::replace(path, seen, &out, |b| check_jpeg(b, dw, dh, Orientation::Normal))?;
    Ok(Saved::Pixels)
}

fn rotate_png(path: &Path, data: &[u8], seen: &atomic::Seen, turn: Orientation) -> Result<Saved> {
    let t = Orientation::from_container(&mut Cursor::new(data)).compose(turn);
    let raw = png::decode(data)?.transformed(t);
    let out = raw.encode(Orientation::Normal, t.swaps_axes())?;
    atomic::replace(path, seen, &out, |b| raw.check(b, Orientation::Normal))?;
    Ok(Saved::Pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{self, Target};
    use crate::fixtures_dir;
    use crate::testutil::TmpDir;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use turbojpeg::{Image, PixelFormat};

    fn copy(tmp: &TmpDir, name: &str) -> PathBuf {
        let p = tmp.path().join(name);
        fs::copy(fixtures_dir().join(name), &p).unwrap();
        p
    }

    /// Decodage complet en RGB, orientation EXIF ignoree : les pixels stockes.
    fn stored_rgb(path: &Path) -> (Vec<u8>, usize, usize) {
        let data = fs::read(path).unwrap();
        let mut dec = Decompressor::new().unwrap();
        let hd = dec.read_header(&data).unwrap();
        let mut px = vec![0u8; hd.width * hd.height * 3];
        dec.decompress(&data, Image { pixels: &mut px[..], width: hd.width, pitch: hd.width * 3, height: hd.height, format: PixelFormat::RGB }).unwrap();
        (px, hd.width, hd.height)
    }

    fn displayed(path: &Path) -> decode::Frame {
        decode::decode_capped(path, Target::Full, usize::MAX).unwrap()
    }

    fn orientation(path: &Path) -> Orientation {
        Orientation::read(path)
    }

    fn exiftool(args: &[&str], path: &Path) -> String {
        let out = Command::new("exiftool").args(args).arg(path).output().expect("exiftool dans le conteneur dev");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn no_temp_left(dir: &Path) {
        let left: Vec<_> = fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(".picme-")).collect();
        assert!(left.is_empty(), "temporaires restants : {left:?}");
    }

    #[test]
    fn lossless_rotation_matches_jpegtran_and_resets_orientation() {
        let tmp = TmpDir::new("rot-jpegtran");
        let p = copy(&tmp, "a.jpg");
        let reference = tmp.path().join("ref.jpg");
        let st = Command::new("jpegtran").args(["-rotate", "90", "-perfect", "-copy", "all", "-outfile"]).arg(&reference).arg(&p).status().unwrap();
        assert!(st.success());
        assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), Saved::Pixels);
        let (got, gw, gh) = stored_rgb(&p);
        let (want, ww, wh) = stored_rgb(&reference);
        assert_eq!((gw, gh), (480, 640));
        assert_eq!((gw, gh, got == want), (ww, wh, true), "pixels identiques a jpegtran -perfect");
        assert_eq!(orientation(&p), Orientation::Normal);
        no_temp_left(tmp.path());
    }

    #[test]
    fn four_quarter_turns_and_two_half_turns_are_identity() {
        let tmp = TmpDir::new("rot-identity");
        let p = copy(&tmp, "a.jpg");
        let before = stored_rgb(&p);
        for _ in 0..4 {
            rotate(&p, Orientation::Rotate90).unwrap();
        }
        assert!(stored_rgb(&p) == before, "4 x R = identite au pixel pres");
        rotate(&p, Orientation::Rotate180).unwrap();
        rotate(&p, Orientation::Rotate180).unwrap();
        assert!(stored_rgb(&p) == before, "2 x 180 = identite");
        rotate(&p, Orientation::Rotate90).unwrap();
        rotate(&p, Orientation::Rotate270).unwrap();
        assert!(stored_rgb(&p) == before, "R puis Shift+R = identite");
    }

    #[test]
    fn existing_orientation_is_composed_and_exif_kept() {
        let tmp = TmpDir::new("rot-orient6");
        let p = copy(&tmp, "c_orient6.jpg");
        let make = exiftool(&["-s3", "-Make"], &p);
        let before = displayed(&p);
        assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), Saved::Pixels);
        assert_eq!(orientation(&p), Orientation::Normal);
        assert_eq!(exiftool(&["-n", "-s3", "-Orientation"], &p), "1");
        assert_eq!(exiftool(&["-s3", "-Make"], &p), make, "EXIF conserve");
        // l'image affichee est l'ancienne tournee d'un quart de tour horaire
        let after = displayed(&p);
        let (want, ww, wh) = Orientation::Rotate90.apply_rgba(before.rgba.clone(), before.w, before.h);
        assert_eq!((after.w, after.h), (ww, wh));
        let diff = after.rgba.iter().zip(&want).map(|(a, b)| u64::from(a.abs_diff(*b))).sum::<u64>() as f64 / want.len() as f64;
        assert!(diff < 2.0, "ecart moyen {diff} : arrondis IDCT seulement");
    }

    #[test]
    fn non_mcu_dimensions_fall_back_to_the_tag() {
        let tmp = TmpDir::new("rot-333");
        let p = copy(&tmp, "b_333x217.jpg");
        let pixels = stored_rgb(&p);
        let before = displayed(&p);
        assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), Saved::Tag);
        assert!(stored_rgb(&p) == pixels, "pixels intacts");
        assert_eq!(orientation(&p), Orientation::Rotate90);
        assert_eq!(exiftool(&["-n", "-s3", "-Orientation"], &p), "6");
        let after = displayed(&p);
        assert_eq!((after.w, after.h), (217, 333));
        assert_eq!(after.rgba, Orientation::Rotate90.apply_rgba(before.rgba, before.w, before.h).0);
        // le tag existe desormais : patch sur place, et retour a 1 apres un tour complet
        assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), Saved::Tag);
        assert_eq!(orientation(&p), Orientation::Rotate180);
        rotate(&p, Orientation::Rotate180).unwrap();
        assert_eq!(orientation(&p), Orientation::Normal);
        assert!(stored_rgb(&p) == pixels);
    }

    #[test]
    fn tag_is_inserted_into_an_exif_without_orientation_and_xmp_follows() {
        let tmp = TmpDir::new("rot-insert");
        let src = copy(&tmp, "b_333x217.jpg");
        let p = tmp.path().join("exif.jpg");
        let st = Command::new("exiftool").args(["-q", "-Make=Canon", "-XMP-dc:Title=titre", "-o"]).arg(&p).arg(&src).status().unwrap();
        assert!(st.success());
        assert_eq!(exiftool(&["-n", "-s3", "-Orientation"], &p), "", "pas de tag au depart");
        assert_eq!(rotate(&p, Orientation::Rotate270).unwrap(), Saved::Tag);
        assert_eq!(exiftool(&["-n", "-s3", "-Orientation"], &p), "8");
        assert_eq!(exiftool(&["-s3", "-Make"], &p), "Canon");
        assert_eq!(exiftool(&["-s3", "-XMP-dc:Title"], &p), "titre");
        assert_eq!(exiftool(&["-validate", "-warning", "-error", "-a", "-s3"], &p), "OK");

        // XMP qui porte aussi l'orientation : suit l'EXIF
        let q = tmp.path().join("xmp.jpg");
        let st = Command::new("exiftool").args(["-q", "-n", "-XMP-tiff:Orientation=6", "-o"]).arg(&q).arg(fixtures_dir().join("c_orient6.jpg")).status().unwrap();
        assert!(st.success());
        rotate(&q, Orientation::Rotate90).unwrap();
        assert_eq!(exiftool(&["-n", "-s3", "-XMP-tiff:Orientation"], &q), "1");
        assert_eq!(exiftool(&["-n", "-s3", "-IFD0:Orientation"], &q), "1");
    }

    /// Segments APPn et COM avant les tables : (marqueur, contenu), et l'offset ou commencent les tables.
    fn segments(data: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
        let (mut out, mut i) = (Vec::new(), 2);
        while data[i] == 0xFF && ((0xE0..=0xEF).contains(&data[i + 1]) || data[i + 1] == 0xFE) {
            let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
            out.push((data[i + 1], data[i + 4..i + 2 + len].to_vec()));
            i += 2 + len;
        }
        (out, i)
    }

    #[test]
    fn exif_xmp_comment_and_icc_survive_both_the_dct_and_the_tag_paths() {
        let tmp = TmpDir::new("rot-meta");
        // profil ICC factice (sa taille en tete) : seuls ses octets comptent
        let mut icc: Vec<u8> = (0..300u32).map(|i| (i * 7 % 256) as u8).collect();
        icc[..4].copy_from_slice(&300u32.to_be_bytes());
        let tags = ["-s3", "-Make", "-Model", "-DateTimeOriginal", "-GPSLatitude", "-XMP-dc:Title", "-Comment"];
        for (name, saved) in [("a.jpg", Saved::Pixels), ("b_333x217.jpg", Saved::Tag)] {
            let src = copy(&tmp, name);
            let p = tmp.path().join(format!("meta-{name}"));
            let st = Command::new("exiftool")
                .args(["-q", "-Make=Canon", "-Model=EOS R6", "-DateTimeOriginal=2024:05:06 07:08:09", "-GPSLatitude=45.5"])
                .args(["-GPSLatitudeRef=N", "-XMP-dc:Title=titre", "-Comment=garde", "-o"])
                .arg(&p)
                .arg(&src)
                .status()
                .unwrap();
            assert!(st.success());
            let mut data = fs::read(&p).unwrap();
            let mut app2 = vec![0xFF, 0xE2];
            app2.extend_from_slice(&u16::try_from(2 + 14 + icc.len()).unwrap().to_be_bytes());
            app2.extend_from_slice(b"ICC_PROFILE\0\x01\x01");
            app2.extend_from_slice(&icc);
            let at = segments(&data).1;
            data.splice(at..at, app2);
            fs::write(&p, &data).unwrap();
            let want = exiftool(&tags, &p);
            // exiftool ne met pas ExifImageWidth/Height dans l'ExifIFD qu'il cree : -validate le signale avant comme apres
            let validate = ["-validate", "-warning", "-error", "-a", "-s3"];
            let report = exiftool(&validate, &p);
            assert_eq!(want.lines().count(), 6, "{want}");
            let icc_of = |d: &[u8]| segments(d).0.into_iter().filter(|(m, _)| *m == 0xE2).collect::<Vec<_>>();
            let icc_before = icc_of(&data);
            assert_eq!(icc_before.len(), 1);

            assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), saved, "{name}");
            assert_eq!(icc_of(&fs::read(&p).unwrap()), icc_before, "{name} : ICC");
            assert_eq!(exiftool(&tags, &p), want, "{name} : EXIF, GPS, XMP, COM");
            assert_eq!(exiftool(&validate, &p), report, "{name}");
        }
    }

    #[test]
    fn written_jpegs_pass_exiftool_validation() {
        let tmp = TmpDir::new("rot-validate");
        for name in ["a.jpg", "b_333x217.jpg", "c_orient6.jpg"] {
            let p = copy(&tmp, name);
            for turn in [Orientation::Rotate270, Orientation::Rotate90, Orientation::Rotate180] {
                rotate(&p, turn).unwrap();
                let report = exiftool(&["-validate", "-warning", "-error", "-a", "-s3"], &p);
                assert_eq!(report, "OK", "{name} apres {turn:?}");
            }
        }
    }

    #[test]
    fn png_rotation_is_lossless_and_keeps_chunks() {
        let tmp = TmpDir::new("rot-png");
        let p = copy(&tmp, "d.png");
        // profil ICC factice dans un iCCP : flux zlib a un bloc stocke (non compresse)
        let icc: Vec<u8> = (0..200u32).map(|i| (i * 11 % 256) as u8).collect();
        let mut iccp = b"factice\0\0\x78\x01\x01".to_vec();
        let n = icc.len() as u16;
        iccp.extend_from_slice(&n.to_le_bytes());
        iccp.extend_from_slice(&(!n).to_le_bytes());
        iccp.extend_from_slice(&icc);
        let (a, b) = icc.iter().fold((1u32, 0u32), |(a, b), &x| ((a + u32::from(x)) % 65521, (b + a + u32::from(x)) % 65521));
        iccp.extend_from_slice(&(b << 16 | a).to_be_bytes());
        let mut data = fs::read(&p).unwrap();
        insert_chunk(&mut data, b"iCCP", &iccp);
        fs::write(&p, &data).unwrap();
        assert_eq!(png_raw(&p).1.icc_profile.as_deref(), Some(&icc[..]));
        let before = displayed(&p);
        assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), Saved::Pixels);
        let after = displayed(&p);
        assert_eq!(after.rgba, Orientation::Rotate90.apply_rgba(before.rgba.clone(), before.w, before.h).0, "sans perte, au pixel pres");
        assert_eq!(png_raw(&p).1.icc_profile.as_deref(), Some(&icc[..]), "iCCP conserve");
        for _ in 0..3 {
            rotate(&p, Orientation::Rotate90).unwrap();
        }
        assert_eq!(displayed(&p).rgba, before.rgba);
        no_temp_left(tmp.path());
    }

    /// Insere un chunk avant le premier IDAT d'un PNG.
    fn insert_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        let at = png.windows(4).position(|w| w == b"IDAT").unwrap() - 4;
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(&c[4..]);
        c.extend_from_slice(&crc.finalize().to_be_bytes());
        png.splice(at..at, c);
    }

    /// Pixels bruts (indices, echantillons 16 bits...) et metadonnees d'un PNG, sans conversion.
    fn png_raw(path: &Path) -> (Vec<u8>, ::png::Info<'static>) {
        let mut dec = ::png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
        dec.set_transformations(::png::Transformations::IDENTITY);
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let n = r.next_frame(&mut buf).unwrap().buffer_size();
        buf.truncate(n);
        (buf, r.info().clone())
    }

    #[test]
    fn png_rotation_keeps_palette_depth_and_chunks() {
        let tmp = TmpDir::new("rot-png-rich");
        let p = tmp.path().join("pal.png");
        // indexe 4 bits, 5 x 3, palette, tRNS, pHYs 100 x 200, tEXt, eXIf Orientation 3
        let (w, h) = (5u32, 3u32);
        let idx: Vec<u8> = (0..w * h).map(|i| (i % 16) as u8).collect();
        let mut buf = Vec::new();
        let mut enc = ::png::Encoder::new(&mut buf, w, h);
        enc.set_color(::png::ColorType::Indexed);
        enc.set_depth(::png::BitDepth::Four);
        enc.set_palette((0..16u8).flat_map(|i| [i * 16, 255 - i * 16, i]).collect::<Vec<u8>>());
        enc.set_trns(vec![0u8, 128]);
        enc.set_pixel_dims(Some(::png::PixelDimensions { xppu: 100, yppu: 200, unit: ::png::Unit::Meter }));
        enc.add_text_chunk("Comment".into(), "garde".into()).unwrap();
        let mut wr = enc.write_header().unwrap();
        let rows: Vec<u8> = idx.chunks(5).flat_map(|r| [r[0] << 4 | r[1], r[2] << 4 | r[3], r[4] << 4]).collect();
        wr.write_image_data(&rows).unwrap();
        wr.finish().unwrap();
        insert_chunk(&mut buf, b"eXIf", &exif::minimal(3));
        fs::write(&p, &buf).unwrap();
        let before = displayed(&p);

        assert_eq!(rotate(&p, Orientation::Rotate90).unwrap(), Saved::Pixels);
        // 180 deja dans le tag, puis un quart de tour : les indices tournent de 270, le tag revient a 1
        let after = displayed(&p);
        assert_eq!(after.rgba, Orientation::Rotate90.apply_rgba(before.rgba, before.w, before.h).0);
        let (raw, info) = png_raw(&p);
        assert_eq!((info.width, info.height, info.bit_depth, info.color_type), (3, 5, ::png::BitDepth::Four, ::png::ColorType::Indexed));
        let want = Orientation::Rotate270.apply_px::<1>(idx, w, h).0;
        let rows: Vec<u8> = want.chunks(3).flat_map(|r| [r[0] << 4 | r[1], r[2] << 4]).collect();
        assert_eq!(raw, rows);
        assert_eq!(info.trns.as_deref(), Some(&[0u8, 128][..]));
        assert_eq!(info.palette.as_ref().unwrap().len(), 48);
        let pd = info.pixel_dims.unwrap();
        assert_eq!((pd.xppu, pd.yppu), (200, 100), "pHYs suit les axes");
        assert_eq!(info.uncompressed_latin1_text[0].text, "garde");
        assert_eq!(exiftool(&["-n", "-s3", "-Orientation"], &p), "1");
    }

    #[test]
    fn png_16_bit_rgba_rotation_is_exact() {
        let tmp = TmpDir::new("rot-png-16");
        let p = tmp.path().join("deep.png");
        let (w, h) = (3u32, 2u32);
        let samples: Vec<u8> = (0..w * h * 8).map(|i| (i * 37 % 251) as u8).collect();
        let mut buf = Vec::new();
        let mut enc = ::png::Encoder::new(&mut buf, w, h);
        enc.set_color(::png::ColorType::Rgba);
        enc.set_depth(::png::BitDepth::Sixteen);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&samples).unwrap();
        wr.finish().unwrap();
        fs::write(&p, &buf).unwrap();
        rotate(&p, Orientation::Rotate270).unwrap();
        let (raw, info) = png_raw(&p);
        assert_eq!((info.width, info.height, info.bit_depth), (2, 3, ::png::BitDepth::Sixteen));
        assert_eq!(raw, Orientation::Rotate270.apply_px::<8>(samples, w, h).0);
    }

    #[test]
    fn other_formats_are_refused_and_untouched() {
        let tmp = TmpDir::new("rot-other");
        for name in ["e.tiff", "f.webp", "g.heic", "i.jxl"] {
            let p = copy(&tmp, name);
            let before = fs::read(&p).unwrap();
            assert!(matches!(rotate(&p, Orientation::Rotate90), Err(Error::Unsupported(_))), "{name}");
            assert_eq!(fs::read(&p).unwrap(), before);
        }
        no_temp_left(tmp.path());
    }

    #[test]
    fn corrupt_jpeg_is_an_error_and_untouched() {
        let tmp = TmpDir::new("rot-corrupt");
        let p = tmp.path().join("x.jpg");
        let mut data = fs::read(fixtures_dir().join("a.jpg")).unwrap();
        data.truncate(data.len() / 3);
        fs::write(&p, &data).unwrap();
        let r = rotate(&p, Orientation::Rotate90);
        assert!(r.is_err(), "{r:?}");
        assert_eq!(fs::read(&p).unwrap(), data, "original intact");
        no_temp_left(tmp.path());
    }

    #[test]
    fn a_file_rewritten_between_the_read_and_the_rename_is_kept() {
        let tmp = TmpDir::new("rot-concurrent");
        for name in ["a.jpg", "d.png"] {
            let p = copy(&tmp, name);
            let (data, seen) = atomic::read(&p).unwrap();
            // un autre programme reecrit le fichier apres notre lecture, avant le rename
            let theirs = [&b"travail d'un autre programme"[..], name.as_bytes()].concat();
            fs::write(&p, &theirs).unwrap();
            let r = rotate_data(&p, data, &seen, Orientation::Rotate90);
            assert!(matches!(r, Err(Error::Invalid(_))), "{name} : {r:?}");
            assert_eq!(fs::read(&p).unwrap(), theirs, "{name} : la version de l'autre programme est intacte");
        }
        no_temp_left(tmp.path());
    }
}
