//! Crop enregistre. Le rectangle est dessine sur l'image affichee, en fractions de ses cotes, et converti en pixels
//! stockes par `map_rect`. JPEG : tjTransform sans perte, origine descendue sur une frontiere de MCU (le bord droit
//! et le bord bas restent ou l'utilisateur les a mis), marqueurs et orientation conserves. PNG et TIFF : meme format,
//! sans perte. Le reste (WebP, HEIF, AVIF, JPEG XL) : JPEG qualite 92, en copie seulement.
//!
//! Par defaut le resultat est une copie `<stem>-crop.<ext>` (puis `<stem>-crop-2.<ext>`, `-crop-3`...). Le
//! remplacement de l'original passe par la corbeille fournie par l'appelant (gio cote UI) : l'original n'est jamais
//! supprime, et si la corbeille echoue rien n'est remplace.

use super::{atomic, check_jpeg, png};
use crate::decode::{self, Target};
use crate::error::{Error, Result};
use crate::media::{self, MediaKind};
use crate::orient::{Orientation, Rect};
use std::ffi::{OsStr, OsString};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use turbojpeg::{Decompressor, Image, PixelFormat, Subsamp, Transform, TransformCrop, Transformer};

/// Qualite du re-encodage JPEG des formats qui ne s'ecrivent pas.
const JPEG_QUALITY: i32 = 92;

/// Rectangle en fractions de l'image affichee (0 a 1, origine en haut a gauche).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frac {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Resultat d'un crop enregistre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cropped {
    /// Fichier ecrit : la copie, ou l'original remplace.
    pub path: PathBuf,
    /// Vrai si l'original a ete remplace (et mis a la corbeille).
    pub replaced: bool,
    /// Faux pour un re-encodage JPEG avec perte.
    pub lossless: bool,
    /// Dimensions affichees du resultat.
    pub w: u32,
    pub h: u32,
}

/// Rectangle affiche en pixels : bords arrondis au pixel le plus proche, au moins un pixel.
fn to_pixels(r: Frac, dw: u32, dh: u32) -> Result<Rect> {
    let edge = |v: f64, d: u32| (v.clamp(0.0, 1.0) * f64::from(d)).round() as u32;
    let (x0, x1) = (edge(r.x, dw), edge(r.x + r.w, dw));
    let (y0, y1) = (edge(r.y, dh), edge(r.y + r.h, dh));
    if !(r.w > 0.0 && r.h > 0.0) {
        return Err(Error::Invalid("rectangle de crop vide".into()));
    }
    let (x0, y0) = (x0.min(dw - 1), y0.min(dh - 1));
    Ok(Rect { x: x0, y: y0, w: x1.max(x0 + 1) - x0, h: y1.max(y0 + 1) - y0 })
}

/// Crop en memoire, pret a ecrire.
struct Out {
    bytes: Vec<u8>,
    /// `None` : extension d'origine (meme format) ; sinon celle du re-encodage.
    ext: Option<&'static str>,
    lossless: bool,
    w: u32,
    h: u32,
    check: Check,
}

/// Controle du fichier ecrit (relecture avant le `rename`).
type Check = Box<dyn Fn(&[u8]) -> Result<()>>;

/// Mise a la corbeille de l'original remplace par son crop.
pub type Trash<'a> = &'a dyn Fn(&Path) -> Result<()>;

fn render(path: &Path, data: Vec<u8>, rect: Frac) -> Result<Out> {
    match media::sniff(&data) {
        Some(MediaKind::Jpeg) => crop_jpeg(data, rect),
        Some(MediaKind::Png) => crop_png(&data, rect),
        Some(MediaKind::Tiff) => crop_tiff(&data, rect),
        Some(MediaKind::Video | MediaKind::Raw) | None => Err(Error::Unsupported("crop impossible pour ce fichier".into())),
        Some(_) => crop_reencoded(path, rect),
    }
}

fn crop_jpeg(data: Vec<u8>, rect: Frac) -> Result<Out> {
    let header = Decompressor::new().map_err(Error::decode)?.read_header(&data).map_err(Error::decode)?;
    let (w, h) = (header.width as u32, header.height as u32);
    let orient = Orientation::from_container(&mut Cursor::new(&data));
    let (dw, dh) = orient.displayed_dims(w, h);
    let s = orient.map_rect(to_pixels(rect, dw, dh)?, w, h);
    let (mw, mh) = header.subsamp.mcu_size();
    let (x, y) = (s.x - s.x % mw as u32, s.y - s.y % mh as u32);
    let (cw, ch) = (s.w + s.x - x, s.h + s.y - y);
    let crop = TransformCrop { x: x as usize, y: y as usize, width: Some(cw as usize), height: Some(ch as usize) };
    let tr = Transform { crop: Some(crop), ..Transform::default() };
    let mut out = Transformer::new().map_err(Error::decode)?.transform_to_vec(&tr, &data).map_err(Error::decode)?;
    super::exif::jpeg_set_pixel_dims(&mut out, cw, ch);
    let (ow, oh) = orient.displayed_dims(cw, ch);
    Ok(Out { bytes: out, ext: None, lossless: true, w: ow, h: oh, check: Box::new(move |b| check_jpeg(b, cw, ch, orient)) })
}

fn crop_png(data: &[u8], rect: Frac) -> Result<Out> {
    let orient = Orientation::from_container(&mut Cursor::new(data));
    let raw = png::decode(data)?;
    let (dw, dh) = orient.displayed_dims(raw.w, raw.h);
    let s = orient.map_rect(to_pixels(rect, dw, dh)?, raw.w, raw.h);
    let raw = raw.cropped(s);
    let bytes = raw.encode(orient, false)?;
    let (w, h) = orient.displayed_dims(s.w, s.h);
    Ok(Out { bytes, ext: None, lossless: true, w, h, check: Box::new(move |b| raw.check(b, orient)) })
}

/// TIFF : orientation appliquee aux pixels (l'encodeur TIFF n'ecrit pas le tag), puis crop du rectangle affiche.
fn crop_tiff(data: &[u8], rect: Frac) -> Result<Out> {
    let orient = Orientation::from_container(&mut Cursor::new(data));
    let mut img = image::load_from_memory_with_format(data, image::ImageFormat::Tiff).map_err(Error::decode)?;
    if let Some(o) = image::metadata::Orientation::from_exif(orient.exif_value() as u8) {
        img.apply_orientation(o);
    }
    let r = to_pixels(rect, img.width(), img.height())?;
    let img = img.crop_imm(r.x, r.y, r.w, r.h);
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Tiff).map_err(Error::decode)?;
    let check = move |b: &[u8]| {
        let back = image::load_from_memory_with_format(b, image::ImageFormat::Tiff).map_err(Error::decode)?;
        if back.as_bytes() != img.as_bytes() || back.color() != img.color() {
            return Err(Error::Invalid("TIFF relu different de l'image ecrite".into()));
        }
        Ok(())
    };
    Ok(Out { bytes: bytes.into_inner(), ext: None, lossless: true, w: r.w, h: r.h, check: Box::new(check) })
}

/// Formats que picme ne sait pas ecrire : decodage complet (orientation appliquee), crop, JPEG qualite 92.
/// La transparence est perdue (JPEG).
fn crop_reencoded(path: &Path, rect: Frac) -> Result<Out> {
    let frame = decode::decode_capped(path, Target::Full, usize::MAX)?;
    let r = to_pixels(rect, frame.w, frame.h)?;
    let line = frame.w as usize * 4;
    let mut rgba = Vec::with_capacity(r.w as usize * r.h as usize * 4);
    for row in frame.rgba.chunks(line).skip(r.y as usize).take(r.h as usize) {
        rgba.extend_from_slice(&row[r.x as usize * 4..(r.x + r.w) as usize * 4]);
    }
    let img = Image { pixels: &rgba[..], width: r.w as usize, pitch: r.w as usize * 4, height: r.h as usize, format: PixelFormat::RGBA };
    let bytes = turbojpeg::compress(img, JPEG_QUALITY, Subsamp::Sub2x2).map_err(Error::decode)?.to_vec();
    let (w, h) = (r.w, r.h);
    Ok(Out { bytes, ext: Some("jpg"), lossless: false, w, h, check: Box::new(move |b| check_jpeg(b, w, h, Orientation::Normal)) })
}

/// `<stem>-crop.<ext>` pour `n` = 1, `<stem>-crop-<n>.<ext>` ensuite.
fn copy_name(path: &Path, ext: &OsStr, n: u32) -> PathBuf {
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(if n == 1 { "-crop".to_string() } else { format!("-crop-{n}") });
    if !ext.is_empty() {
        name.push(".");
        name.push(ext);
    }
    path.with_file_name(name)
}

/// Pose le temporaire sous le premier nom de copie libre.
fn commit_copy(staged: &mut atomic::Staged, path: &Path, ext: &OsStr) -> Result<PathBuf> {
    for n in 1..10_000 {
        let target = copy_name(path, ext, n);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        match staged.commit_new(&target) {
            Ok(()) => return Ok(target),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Error::Invalid(format!("aucun nom de copie libre pour {}", path.display())))
}

/// Crop du rectangle `rect` de l'image `path`. Copie a cote par defaut ; avec `replace`, l'original part dans la
/// corbeille par cette fonction et le crop prend sa place (meme nom), sauf pour un format re-encode en JPEG qui
/// reste en copie. Atomique : en cas d'erreur, ni l'original ni un fichier partiel ne sont touches.
pub fn crop(path: &Path, rect: Frac, replace: Option<Trash>) -> Result<Cropped> {
    let path = std::fs::canonicalize(path)?;
    let out = render(&path, std::fs::read(&path)?, rect)?;
    let ext: OsString = match out.ext {
        Some(e) => e.into(),
        None => path.extension().unwrap_or_default().to_os_string(),
    };
    let first = copy_name(&path, &ext, 1);
    let mut staged = atomic::stage_bytes(&first, Some(&path), &out.bytes, &*out.check)?;
    let done = |path: PathBuf, replaced: bool| Cropped { path, replaced, lossless: out.lossless, w: out.w, h: out.h };
    match replace {
        Some(trash) if out.ext.is_none() => {
            // corbeille d'abord : si elle echoue, le temporaire disparait et l'original reste
            trash(&path)?;
            match staged.commit(&path) {
                Ok(()) => Ok(done(path, true)),
                // l'original est deja a la corbeille : le crop ne doit pas se perdre, il part en copie
                Err(_) => Ok(done(commit_copy(&mut staged, &path, &ext)?, false)),
            }
        }
        _ => Ok(done(commit_copy(&mut staged, &path, &ext)?, false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures_dir;
    use crate::testutil::TmpDir;
    use std::cell::RefCell;
    use std::fs;

    fn copy(tmp: &TmpDir, name: &str) -> PathBuf {
        let p = tmp.path().join(name);
        fs::copy(fixtures_dir().join(name), &p).unwrap();
        p
    }

    fn displayed(path: &Path) -> decode::Frame {
        decode::decode_capped(path, Target::Full, usize::MAX).unwrap()
    }

    fn sub(f: &decode::Frame, r: Rect) -> Vec<u8> {
        let mut v = Vec::new();
        for row in f.rgba.chunks(f.w as usize * 4).skip(r.y as usize).take(r.h as usize) {
            v.extend_from_slice(&row[r.x as usize * 4..(r.x + r.w) as usize * 4]);
        }
        v
    }

    const HALF: Frac = Frac { x: 0.25, y: 0.25, w: 0.5, h: 0.5 };

    #[test]
    fn fractions_round_to_the_nearest_pixel() {
        assert_eq!(to_pixels(HALF, 640, 480).unwrap(), Rect { x: 160, y: 120, w: 320, h: 240 });
        assert_eq!(to_pixels(Frac { x: 0.9999, y: 0.0, w: 0.5, h: 1.0 }, 10, 10).unwrap(), Rect { x: 9, y: 0, w: 1, h: 10 });
        assert!(to_pixels(Frac { x: 0.1, y: 0.1, w: 0.0, h: 0.5 }, 10, 10).is_err());
    }

    #[test]
    fn jpeg_crop_is_lossless_and_aligned_on_the_mcu() {
        let tmp = TmpDir::new("crop-jpeg");
        let p = copy(&tmp, "a.jpg");
        let before = fs::read(&p).unwrap();
        // origine 100,50 : descend sur 96,48 (MCU 16x16 en 4:2:0), bords droit et bas a 420 et 290
        let r = Frac { x: 100.0 / 640.0, y: 50.0 / 480.0, w: 320.0 / 640.0, h: 240.0 / 480.0 };
        let c = crop(&p, r, None).unwrap();
        assert_eq!(c.path, tmp.path().canonicalize().unwrap().join("a-crop.jpg"));
        assert!(c.lossless && !c.replaced);
        assert_eq!((c.w, c.h), (324, 242));
        assert_eq!(fs::read(&p).unwrap(), before, "copie : original intact");
        // sans perte : memes blocs DCT que l'original, donc pixels identiques au crop d'un decodage complet
        // (a l'exception des bords de MCU interieurs, que le decodeur lisse en 4:2:0 : on compare l'interieur)
        let full = displayed(&p);
        let got = displayed(&c.path);
        let inner = Rect { x: 16, y: 16, w: 324 - 32, h: 242 - 32 };
        let want = sub(&full, Rect { x: 96 + 16, y: 48 + 16, w: inner.w, h: inner.h });
        assert_eq!(sub(&got, inner), want);
        // deuxieme copie : nom suivant
        assert_eq!(crop(&p, r, None).unwrap().path.file_name().unwrap(), "a-crop-2.jpg");
    }

    #[test]
    fn jpeg_crop_follows_the_displayed_orientation() {
        let tmp = TmpDir::new("crop-orient");
        let p = copy(&tmp, "c_orient6.jpg");
        let full = displayed(&p);
        assert_eq!((full.w, full.h), (480, 640));
        // quart superieur gauche de l'affiche
        let c = crop(&p, Frac { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }, None).unwrap();
        assert_eq!((c.w, c.h), (240, 320));
        assert_eq!(Orientation::read(&c.path), Orientation::Rotate90, "orientation conservee");
        let got = displayed(&c.path);
        let inner = Rect { x: 0, y: 0, w: 224, h: 304 };
        assert_eq!(sub(&got, inner), sub(&full, inner));
    }

    #[test]
    fn png_and_tiff_crops_are_exact() {
        let tmp = TmpDir::new("crop-png-tiff");
        for name in ["d.png", "e.tiff"] {
            let p = copy(&tmp, name);
            let full = displayed(&p);
            let c = crop(&p, HALF, None).unwrap();
            assert!(c.lossless);
            assert_eq!(c.path.extension(), p.extension());
            let r = to_pixels(HALF, full.w, full.h).unwrap();
            let got = displayed(&c.path);
            assert_eq!((got.w, got.h), (r.w, r.h));
            assert_eq!(got.rgba, sub(&full, r), "{name}");
        }
    }

    #[test]
    fn other_formats_become_a_jpeg_copy_even_when_replacing() {
        let tmp = TmpDir::new("crop-webp");
        let p = copy(&tmp, "f.webp");
        let trashed = RefCell::new(Vec::new());
        let trash = |p: &Path| {
            trashed.borrow_mut().push(p.to_path_buf());
            Ok(())
        };
        let c = crop(&p, HALF, Some(&trash)).unwrap();
        assert_eq!(c.path.file_name().unwrap(), "f-crop.jpg");
        assert!(!c.lossless && !c.replaced);
        assert!(trashed.borrow().is_empty(), "jamais de remplacement par un re-encodage");
        assert!(p.exists());
        let full = displayed(&p);
        let got = displayed(&c.path);
        assert_eq!((got.w, got.h), (full.w / 2, full.h / 2));
    }

    #[test]
    fn replace_trashes_the_original_then_takes_its_name() {
        let tmp = TmpDir::new("crop-replace");
        let p = copy(&tmp, "a.jpg");
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let trash = |p: &Path| Ok(fs::rename(p, bin.join(p.file_name().unwrap()))?);
        let c = crop(&p, HALF, Some(&trash)).unwrap();
        assert!(c.replaced);
        assert_eq!(c.path, p.canonicalize().unwrap());
        assert_eq!(fs::read(bin.join("a.jpg")).unwrap(), fs::read(fixtures_dir().join("a.jpg")).unwrap(), "original intact a la corbeille");
        // y = 120 descend sur 112 (MCU de 16) : 8 lignes de plus en haut
        assert_eq!((displayed(&p).w, displayed(&p).h), (320, 248));
    }

    #[test]
    fn failed_trash_replaces_nothing_and_leaves_no_temp() {
        let tmp = TmpDir::new("crop-trash-fail");
        let p = copy(&tmp, "a.jpg");
        let before = fs::read(&p).unwrap();
        let trash = |_: &Path| Err(Error::Io(io::Error::other("pas de corbeille sur ce systeme de fichiers")));
        assert!(crop(&p, HALF, Some(&trash)).is_err());
        assert_eq!(fs::read(&p).unwrap(), before);
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![OsString::from("a.jpg")]);
    }
}
