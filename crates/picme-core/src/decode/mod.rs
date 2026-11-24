//! Decode vers RGBA8, orientation deja appliquee.
//!
//! Chaque format a son module ; tous passent par le meme plan de sortie (`Plan`) : les dimensions cibles se
//! calculent dans l'espace affiche, puis se convertissent dans l'espace stocke pour reduire AVANT d'orienter
//! (la rotation travaille sur moins de pixels). Aucun `catch_unwind` ici : c'est a l'appelant d'envelopper.

mod anim;
mod heif;
mod jpeg;
mod jxl;
#[cfg(feature = "raw")]
mod raw;
mod raster;
mod resize;

use crate::error::{Error, Result};
use crate::media::{self, MediaKind};
use crate::orient::Orientation;
use std::path::Path;

pub use anim::{AnimFrame, Animation, MAX_ANIM_BYTES, MAX_ANIM_FRAMES, Outcome as AnimOutcome, decode_animation};

/// Cote de la boite du `Preview` : celle de la miniature freedesktop `large`.
pub const PREVIEW_SIZE: u32 = 256;

/// Boite d'une miniature de filmstrip : 96 px de haut, 192 de large au plus (un panorama ne s'etire pas).
pub const STRIP_W: u32 = 192;
pub const STRIP_H: u32 = 96;

/// Budget RAM par defaut du cache de textures (`--cache-mb`, 256 Mo). `Full` est reduit au-dela de la moitie.
pub const DEFAULT_BUDGET_BYTES: usize = 256 << 20;

/// Plafond de pixels (largeur x hauteur) d'une image que les decodeurs sans reduction a la source (HEIF, JPEG XL)
/// acceptent : 200 Mpx, soit 800 Mo en RGBA. Au-dela, l'image est refusee avant toute allocation.
pub const MAX_PIXELS: u64 = 200_000_000;

/// Plafond en octets RGBA du decode d'une image entiere pour l'edition (`MAX_PIXELS` pixels).
pub const MAX_PIXELS_BYTES: usize = MAX_PIXELS as usize * 4;

/// Refuse une image de plus de `MAX_PIXELS` pixels, a appeler sur les dimensions de l'en-tete avant d'allouer.
pub(crate) fn check_dims(w: u32, h: u32) -> Result<()> {
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(Error::Decode(format!("image de {w}x{h} pixels : au-dela du plafond de {} Mpx", MAX_PIXELS / 1_000_000)));
    }
    Ok(())
}

/// Taille voulue. Dans tous les cas l'image garde ses proportions et n'est jamais agrandie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Premier affichage, le plus vite possible : tient dans `PREVIEW_SIZE` x `PREVIEW_SIZE`. Pour un HEIF,
    /// la thumbnail embarquee si elle existe, donc eventuellement plus petite que la boite (voir `Frame::w`).
    /// Une miniature freedesktop `large` doit venir de `Fit(256, 256)`, pas de `Preview`.
    Preview,
    /// Miniature du filmstrip : tient dans `STRIP_W` x `STRIP_H`. Cle distincte de `Preview` pour que les resultats
    /// du filmstrip et ceux du controller ne se melangent pas dans le pool `thumbs` ; le travail du pool la tire de
    /// la miniature freedesktop (`Frame::shrink_to`), `decode` la sait aussi produire directement.
    Strip,
    /// Tient dans une fenetre de `w` x `h` pixels (device pixels).
    Fit(u32, u32),
    /// Pleine resolution, reduite si elle depasse la moitie du budget (voir `decode_capped`).
    Full,
}

/// Image decodee. `w` x `h` : dimensions du buffer, orientation appliquee. `full_w` x `full_h` : dimensions de
/// l'image entiere dans le meme espace (orientee), pour savoir de combien le buffer est reduit.
#[derive(Debug, Clone)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub full_w: u32,
    pub full_h: u32,
    /// Vrai pour une miniature de cache plus petite que ce qui etait demande alors que l'image a plus de pixels :
    /// elle n'est jamais agrandie, c'est l'affichage qui l'etire. Toujours faux pour un decode.
    pub low_res: bool,
    /// Vrai pour un JPEG tronque ou corrompu decode par `decode_display` : une partie de l'image est grise. Une
    /// frame partielle ne sert qu'a l'affichage (ni edition, ni miniature ecrite sur le disque).
    pub partial: bool,
}

impl Frame {
    /// Octets du buffer RGBA : ce que le cache LRU compte.
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }

    /// Reduit le buffer pour qu'il tienne dans `bw` x `bh` (jamais agrandi) ; `full_w` x `full_h` ne changent pas.
    pub fn shrink_to(self, bw: u32, bh: u32) -> Result<Frame> {
        let (nw, nh) = out_dims(self.w, self.h, Target::Fit(bw, bh), usize::MAX);
        if (nw, nh) == (self.w, self.h) {
            return Ok(self);
        }
        let rgba = resize::resize_rgba(self.rgba, self.w, self.h, nw, nh, true)?;
        Ok(Frame { rgba, w: nw, h: nh, full_w: self.full_w, full_h: self.full_h, low_res: self.low_res, partial: self.partial })
    }
}

/// Memes proportions, a l'arrondi d'un pixel de la petite image pres.
pub(crate) fn ratio_matches(w: u32, h: u32, full_w: u32, full_h: u32) -> bool {
    let (a, b) = (w as u64 * full_h as u64, h as u64 * full_w as u64);
    a.abs_diff(b) <= full_w.max(full_h) as u64
}

/// Dimensions de l'image entiere, orientation appliquee (celles de `Frame::full_w` et `full_h`), lues dans l'en-tete
/// sans decoder les pixels. `None` si le format n'en donne pas a bon compte (JXL, RAW, video) ou si le fichier est
/// illisible.
pub fn probe_dims(path: &Path) -> Option<(u32, u32)> {
    match media::detect(path)? {
        MediaKind::Jpeg => jpeg::probe(path),
        kind @ (MediaKind::Png
        | MediaKind::Webp
        | MediaKind::Tiff
        | MediaKind::Gif
        | MediaKind::Bmp
        | MediaKind::Ico
        | MediaKind::Qoi
        | MediaKind::Pnm) => raster::probe(path, kind),
        MediaKind::Heif => heif::probe(path),
        MediaKind::Jxl | MediaKind::Raw | MediaKind::Video => None,
    }
}

/// Decode `path` a la taille `target`, avec le plafond de `Full` du budget par defaut.
pub fn decode(path: &Path, target: Target) -> Result<Frame> {
    decode_capped(path, target, DEFAULT_BUDGET_BYTES / 2)
}

/// Comme `decode`, avec un plafond explicite (en octets RGBA) pour `Full` : au-dela, l'image est reduite de
/// facon a tenir dans `max_full_bytes`. Le plafond vaut la moitie du budget du cache (`--cache-mb`).
/// Un fichier tronque ou corrompu est une erreur : c'est ce que l'edition (crop) doit lire.
pub fn decode_capped(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    decode_with(path, target, max_full_bytes, false)
}

/// Comme `decode_capped`, pour l'AFFICHAGE seulement : un JPEG tronque ou aux donnees corrompues rend ce que libjpeg
/// a pu lire, `Frame::partial` vrai (le reste de l'image est gris). Cette `Frame` n'est jamais une entree d'edition
/// ni une miniature a ecrire.
pub fn decode_display(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    decode_with(path, target, max_full_bytes, true)
}

fn decode_with(path: &Path, target: Target, max_full_bytes: usize, lenient: bool) -> Result<Frame> {
    if let Target::Fit(0, _) | Target::Fit(_, 0) = target {
        return Err(Error::Invalid("Fit demande une fenetre vide".into()));
    }
    match media::detect(path) {
        Some(MediaKind::Jpeg) => jpeg::decode(path, target, max_full_bytes, lenient),
        Some(
            kind @ (MediaKind::Png
            | MediaKind::Webp
            | MediaKind::Tiff
            | MediaKind::Gif
            | MediaKind::Bmp
            | MediaKind::Ico
            | MediaKind::Qoi
            | MediaKind::Pnm),
        ) => raster::decode(path, kind, target, max_full_bytes),
        Some(MediaKind::Heif) => heif::decode(path, target, max_full_bytes),
        Some(MediaKind::Jxl) => jxl::decode(path, target, max_full_bytes),
        #[cfg(feature = "raw")]
        Some(MediaKind::Raw) => raw::decode(path, target, max_full_bytes),
        #[cfg(not(feature = "raw"))]
        Some(MediaKind::Raw) => Err(Error::Unsupported("RAW".into())),
        Some(MediaKind::Video) => Err(Error::Unsupported("video".into())),
        None => Err(Error::Unsupported(format!("type inconnu : {}", path.display()))),
    }
}

/// Dimensions de sortie, dans l'espace affiche, pour une image entiere de `fw` x `fh`.
fn out_dims(fw: u32, fh: u32, target: Target, max_full_bytes: usize) -> (u32, u32) {
    let (bw, bh) = match target {
        Target::Preview => (PREVIEW_SIZE, PREVIEW_SIZE),
        Target::Strip => (STRIP_W, STRIP_H),
        Target::Fit(w, h) => (w, h),
        Target::Full => {
            let bytes = fw as f64 * fh as f64 * 4.0;
            if bytes <= max_full_bytes as f64 {
                return (fw, fh);
            }
            let s = (max_full_bytes as f64 / bytes).sqrt();
            return (((fw as f64 * s) as u32).max(1), ((fh as f64 * s) as u32).max(1));
        }
    };
    if fw <= bw && fh <= bh {
        return (fw, fh);
    }
    let s = (bw as f64 / fw as f64).min(bh as f64 / fh as f64);
    (((fw as f64 * s).round() as u32).clamp(1, bw), ((fh as f64 * s).round() as u32).clamp(1, bh))
}

/// Dimensions a viser pour une image stockee en `sw` x `sh` et orientee par `orient`.
struct Plan {
    full_w: u32,
    full_h: u32,
    /// Cible dans l'espace stocke (celle de l'espace affiche, echangee pour EXIF 5 a 8).
    stored_w: u32,
    stored_h: u32,
}

impl Plan {
    fn new(sw: u32, sh: u32, orient: Orientation, target: Target, max_full_bytes: usize) -> Plan {
        let (full_w, full_h) = orient.displayed_dims(sw, sh);
        let (ow, oh) = out_dims(full_w, full_h, target, max_full_bytes);
        let (stored_w, stored_h) = if orient.swaps_axes() { (oh, ow) } else { (ow, oh) };
        Plan { full_w, full_h, stored_w, stored_h }
    }

    /// Reduit le buffer stocke (`w` x `h`) a la cible, puis l'oriente.
    fn finish(&self, rgba: Vec<u8>, w: u32, h: u32, has_alpha: bool, orient: Orientation) -> Result<Frame> {
        let rgba = resize::resize_rgba(rgba, w, h, self.stored_w, self.stored_h, has_alpha)?;
        let (rgba, w, h) = orient.apply_rgba(rgba, self.stored_w, self.stored_h);
        Ok(Frame { rgba, w, h, full_w: self.full_w, full_h: self.full_h, low_res: false, partial: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures_dir;

    fn fx(name: &str) -> std::path::PathBuf {
        fixtures_dir().join(name)
    }

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = (y * f.w + x) as usize * 4;
        f.rgba[i..i + 4].try_into().unwrap()
    }

    fn is_red(p: [u8; 4]) -> bool {
        p[0] > 200 && p[1] < 70 && p[2] < 70 && p[3] == 255
    }

    /// Ecart moyen absolu sur les canaux RGB entre deux frames de meme taille.
    fn mean_abs_diff(a: &Frame, b: &Frame) -> f64 {
        assert_eq!((a.w, a.h), (b.w, b.h));
        let (mut sum, mut n) = (0u64, 0u64);
        for (pa, pb) in a.rgba.as_chunks::<4>().0.iter().zip(b.rgba.as_chunks::<4>().0) {
            for c in 0..3 {
                sum += pa[c].abs_diff(pb[c]) as u64;
                n += 1;
            }
        }
        sum as f64 / n as f64
    }

    #[test]
    fn out_dims_keeps_proportions_and_never_upscales() {
        let big = usize::MAX;
        assert_eq!(out_dims(640, 480, Target::Fit(320, 240), big), (320, 240));
        assert_eq!(out_dims(640, 480, Target::Fit(1000, 1000), big), (640, 480));
        assert_eq!(out_dims(640, 480, Target::Fit(100, 100), big), (100, 75));
        assert_eq!(out_dims(640, 480, Target::Fit(1000, 300), big), (400, 300));
        assert_eq!(out_dims(640, 480, Target::Preview, big), (256, 192));
        assert_eq!(out_dims(480, 640, Target::Preview, big), (192, 256));
        assert_eq!(out_dims(333, 217, Target::Preview, big), (256, 167));
        assert_eq!(out_dims(640, 480, Target::Full, big), (640, 480));
        assert_eq!(out_dims(10_000, 1, Target::Fit(100, 100), big), (100, 1));
    }

    #[test]
    fn strip_fits_96_px_high_and_a_frame_shrinks_to_it() {
        let big = usize::MAX;
        assert_eq!(out_dims(640, 480, Target::Strip, big), (128, 96));
        assert_eq!(out_dims(480, 640, Target::Strip, big), (72, 96));
        assert_eq!(out_dims(3000, 1000, Target::Strip, big), (192, 64), "un panorama est plafonne en largeur");
        assert_eq!(out_dims(100, 50, Target::Strip, big), (100, 50), "jamais agrandie");

        // la miniature freedesktop (256 x 192) donne la miniature de filmstrip sans passer par le decode
        let preview = decode(&fx("a.jpg"), Target::Preview).unwrap();
        let strip = preview.clone().shrink_to(STRIP_W, STRIP_H).unwrap();
        assert_eq!((strip.w, strip.h, strip.full_w, strip.full_h), (128, 96, 640, 480));
        assert_eq!(strip.rgba.len(), 128 * 96 * 4);
        assert!(is_red(px(&strip, 2, 2)));
        let same = strip.clone().shrink_to(STRIP_W, STRIP_H).unwrap();
        assert_eq!(same.rgba, strip.rgba, "deja assez petite : rendue telle quelle");
        let direct = decode(&fx("a.jpg"), Target::Strip).unwrap();
        assert_eq!((direct.w, direct.h), (128, 96));
    }

    #[test]
    fn full_is_capped_to_the_byte_limit() {
        assert_eq!(out_dims(640, 480, Target::Full, 640 * 480 * 4), (640, 480));
        assert_eq!(out_dims(640, 480, Target::Full, 640 * 480), (320, 240));
        let (w, h) = out_dims(6000, 4000, Target::Full, 128 << 20);
        assert!(w as usize * h as usize * 4 <= 128 << 20);
        assert!(w > 5000, "{w}x{h}");
    }

    #[test]
    fn every_fixture_decodes_at_the_three_targets() {
        type Dims = (u32, u32);
        let cases: &[(&str, Dims, Dims, Dims, Dims)] = &[
            // (fichier, entier orienté, Preview, Fit(320, 240), Fit(1000, 1000))
            ("a.jpg", (640, 480), (256, 192), (320, 240), (640, 480)),
            ("b_333x217.jpg", (333, 217), (256, 167), (320, 209), (333, 217)),
            ("c_orient6.jpg", (480, 640), (192, 256), (180, 240), (480, 640)),
            ("d.png", (640, 480), (256, 192), (320, 240), (640, 480)),
            ("e.tiff", (640, 480), (256, 192), (320, 240), (640, 480)),
            ("f.webp", (640, 480), (256, 192), (320, 240), (640, 480)),
            ("g.heic", (640, 480), (256, 192), (320, 240), (640, 480)),
            ("h.avif", (640, 480), (256, 192), (320, 240), (640, 480)),
            ("i.jxl", (640, 480), (256, 192), (320, 240), (640, 480)),
        ];
        for (name, full, preview, fit, fit_big) in cases {
            for (target, want) in [
                (Target::Preview, *preview),
                (Target::Fit(320, 240), *fit),
                (Target::Fit(1000, 1000), *fit_big),
                (Target::Full, *full),
            ] {
                let f = decode(&fx(name), target).unwrap_or_else(|e| panic!("{name} {target:?} : {e}"));
                assert_eq!((f.w, f.h), want, "{name} {target:?}");
                assert_eq!((f.full_w, f.full_h), *full, "{name} {target:?}");
                assert_eq!(f.rgba.len(), f.w as usize * f.h as usize * 4, "{name} {target:?}");
            }
        }
    }

    #[test]
    fn orientation_6_is_read_and_applied() {
        let f = decode(&fx("c_orient6.jpg"), Target::Full).unwrap();
        assert_eq!((f.w, f.h), (480, 640));
        // le rectangle rouge stocke en haut a gauche (0..120 x 0..80) passe en haut a droite (400..480 x 0..120)
        assert!(is_red(px(&f, 480 - 1 - 10, 10)), "{:?}", px(&f, 469, 10));
        assert!(is_red(px(&f, 480 - 1 - 70, 110)), "{:?}", px(&f, 409, 110));
        assert!(!is_red(px(&f, 480 - 1 - 10, 200)));
        // pixel pour pixel : a.jpg a les memes pixels stockes, tourne d'un quart de tour horaire
        let a = decode(&fx("a.jpg"), Target::Full).unwrap();
        let (rot, w, h) = Orientation::Rotate90.apply_rgba(a.rgba, a.w, a.h);
        assert_eq!((w, h), (480, 640));
        assert_eq!(rot, f.rgba);
    }

    #[test]
    fn orientation_applies_after_scaling() {
        // Fit dans l'espace affiche (portrait) : le facteur n/8 est choisi sur les dimensions stockees
        let f = decode(&fx("c_orient6.jpg"), Target::Fit(120, 160)).unwrap();
        assert_eq!((f.w, f.h, f.full_w, f.full_h), (120, 160, 480, 640));
        assert!(is_red(px(&f, 120 - 1 - 3, 3)));
    }

    #[test]
    fn dct_scaled_jpeg_matches_a_full_decode_then_resize() {
        let small = decode(&fx("a.jpg"), Target::Fit(80, 60)).unwrap();
        assert_eq!((small.w, small.h), (80, 60));
        let full = decode(&fx("a.jpg"), Target::Full).unwrap();
        let reduced = resize::resize_rgba(full.rgba.clone(), 640, 480, 80, 60, false).unwrap();
        let reduced = Frame { rgba: reduced, w: 80, h: 60, full_w: 640, full_h: 480, low_res: false, partial: false };
        let d = mean_abs_diff(&small, &reduced);
        assert!(d < 6.0, "ecart moyen {d}");
    }

    #[test]
    fn all_formats_show_the_same_picture() {
        let reference = decode(&fx("d.png"), Target::Full).unwrap();
        assert!(is_red(px(&reference, 10, 10)));
        assert_eq!(decode(&fx("e.tiff"), Target::Full).unwrap().rgba, reference.rgba, "TIFF sans perte");
        for (name, max_diff) in [("a.jpg", 8.0), ("f.webp", 8.0), ("g.heic", 12.0), ("k_thumb.heic", 12.0), ("h.avif", 16.0), ("i.jxl", 8.0)] {
            let f = decode(&fx(name), Target::Full).unwrap();
            assert!(is_red(px(&f, 10, 10)), "{name} : {:?}", px(&f, 10, 10));
            assert!(!is_red(px(&f, 600, 450)), "{name}");
            let d = mean_abs_diff(&f, &reference);
            eprintln!("{name} : ecart moyen {d:.2}");
            assert!(d < max_diff, "{name} : ecart moyen {d}");
        }
    }

    #[test]
    fn content_picks_the_decoder_whatever_the_name() {
        use image::ImageFormat as F;
        let dir = crate::testutil::TmpDir::new("decode-by-content");
        let mut img = image::RgbImage::new(40, 30);
        for p in img.pixels_mut() {
            *p = image::Rgb([230, 20, 20]);
        }
        let img = image::DynamicImage::ImageRgb8(img);
        for (format, kind) in [(F::Gif, MediaKind::Gif), (F::Bmp, MediaKind::Bmp), (F::Ico, MediaKind::Ico), (F::Qoi, MediaKind::Qoi), (F::Pnm, MediaKind::Pnm)] {
            let mut bytes = std::io::Cursor::new(Vec::new());
            // l'encodeur ICO n'ecrit un PNG relisible qu'en RGBA
            let src = if format == F::Ico { image::DynamicImage::ImageRgba8(img.to_rgba8()) } else { img.clone() };
            src.write_to(&mut bytes, format).unwrap_or_else(|e| panic!("{format:?} : {e}"));
            // nom neutre, extension inconnue, extension d'un autre format : le contenu decide
            for name in [format!("x-{kind:?}.ithmb"), format!("y-{kind:?}.jpg"), format!("z-{kind:?}")] {
                let p = dir.path().join(&name);
                std::fs::write(&p, bytes.get_ref()).unwrap();
                assert_eq!(media::detect(&p), Some(kind), "{name}");
                let f = decode(&p, Target::Full).unwrap_or_else(|e| panic!("{name} : {e}"));
                assert_eq!((f.w, f.h), (40, 30), "{name}");
                assert!(is_red(px(&f, 5, 5)), "{name} : {:?}", px(&f, 5, 5));
                assert_eq!(probe_dims(&p), Some((40, 30)), "{name}");
            }
        }
        // un PNG et un JPEG sous des noms qui se contredisent
        for (src, ext) in [("d.png", "jpg"), ("a.jpg", "png"), ("a.jpg", "ithmb"), ("f.webp", "tif")] {
            let p = dir.path().join(format!("{src}.{ext}"));
            std::fs::copy(fx(src), &p).unwrap();
            let f = decode(&p, Target::Preview).unwrap_or_else(|e| panic!("{src} en .{ext} : {e}"));
            assert_eq!((f.full_w, f.full_h), (640, 480), "{src} en .{ext}");
        }
        // du texte sous une extension d'image : une erreur, pas un panic
        let txt = dir.path().join("texte.jpg");
        std::fs::write(&txt, b"juste du texte").unwrap();
        assert!(decode(&txt, Target::Preview).is_err());
    }

    #[test]
    fn heif_preview_uses_the_embedded_thumbnail() {
        let f = decode(&fx("k_thumb.heic"), Target::Preview).unwrap();
        assert_eq!((f.w, f.h), (128, 96), "thumbnail embarquee, pas un decode de 640x480");
        assert_eq!((f.full_w, f.full_h), (640, 480));
        assert!(is_red(px(&f, 5, 5)));
        // sans thumbnail : decode complet puis reduction a la boite du Preview
        let g = decode(&fx("g.heic"), Target::Preview).unwrap();
        assert_eq!((g.w, g.h), (256, 192));
        // les autres cibles ignorent la thumbnail
        let fit = decode(&fx("k_thumb.heic"), Target::Fit(320, 240)).unwrap();
        assert_eq!((fit.w, fit.h), (320, 240));
    }

    #[test]
    fn heif_preview_reduces_an_oversized_embedded_thumbnail() {
        // thumbnail embarquee de 320x240 (cas des iPhone) : reduite a la boite de 256, full_* reste celui de l'image
        let f = decode(&fx("l_thumb320.heic"), Target::Preview).unwrap();
        assert_eq!((f.w, f.h, f.full_w, f.full_h), (256, 192, 640, 480));
        assert!(is_red(px(&f, 5, 5)));
    }

    #[test]
    fn full_is_reduced_over_the_cap() {
        let f = decode_capped(&fx("a.jpg"), Target::Full, 640 * 480).unwrap();
        assert_eq!((f.w, f.h, f.full_w, f.full_h), (320, 240, 640, 480));
        let f = decode_capped(&fx("d.png"), Target::Full, 640 * 480).unwrap();
        assert_eq!((f.w, f.h), (320, 240));
    }

    #[test]
    fn probe_dims_matches_the_full_size_of_a_decode_without_decoding() {
        // orient6 : tournee de 90 degres, les dimensions sont celles de l'affichage ; heic/avif : irot de libheif
        for name in ["a.jpg", "b_333x217.jpg", "c_orient6.jpg", "d.png", "e.tiff", "f.webp", "g.heic", "h.avif", "k_thumb.heic"] {
            let f = decode(&fx(name), Target::Preview).unwrap();
            assert_eq!(probe_dims(&fx(name)), Some((f.full_w, f.full_h)), "{name}");
        }
        // pas d'en-tete a bon compte : JXL, video ; fichier absent ou inconnu
        assert_eq!(probe_dims(&fx("i.jxl")), None);
        assert_eq!(probe_dims(&fx("j.mov")), None);
        assert_eq!(probe_dims(&fx("absent.jpg")), None);
    }

    #[test]
    fn probe_dims_reads_a_jpeg_whose_header_is_past_the_first_64_kib() {
        // gros APP2 avant le SOF : le fichier est relu en entier
        let dir = crate::testutil::TmpDir::new("decode-probe-big");
        let src = std::fs::read(fx("a.jpg")).unwrap();
        let mut out = src[..2].to_vec(); // SOI
        let pad = vec![0u8; 65_500];
        out.extend_from_slice(&[0xFF, 0xE2]);
        out.extend_from_slice(&(pad.len() as u16 + 2).to_be_bytes());
        out.extend_from_slice(&pad);
        out.extend_from_slice(&src[2..]);
        let p = dir.path().join("big-header.jpg");
        std::fs::write(&p, &out).unwrap();
        assert_eq!(probe_dims(&p), Some((640, 480)));
    }

    #[test]
    fn refuses_video_raw_and_unknown() {
        assert!(matches!(decode(&fx("j.mov"), Target::Full), Err(Error::Unsupported(_))));
        let dir = crate::testutil::TmpDir::new("decode-unsupported");
        let txt = dir.path().join("a.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(matches!(decode(&txt, Target::Full), Err(Error::Unsupported(_))));
        assert!(matches!(decode(&fx("a.jpg"), Target::Fit(0, 10)), Err(Error::Invalid(_))));
    }

    #[test]
    fn a_truncated_jpeg_is_shown_partial_but_never_decoded_for_editing() {
        let dir = crate::testutil::TmpDir::new("decode-truncated");
        let whole = std::fs::read(fx("a.jpg")).unwrap();
        for (name, keep) in [("third.jpg", whole.len() / 3), ("minus2.jpg", whole.len() - 2)] {
            let p = dir.path().join(name);
            std::fs::write(&p, &whole[..keep]).unwrap();
            for target in [Target::Preview, Target::Fit(320, 240), Target::Full] {
                let err = decode_capped(&p, target, usize::MAX).unwrap_err();
                assert!(matches!(&err, Error::Decode(m) if m.contains("TurboJPEG")), "{name} {target:?}: {err}");
                let f = decode_display(&p, target, usize::MAX).unwrap();
                assert!(f.partial, "{name} {target:?}");
                assert_eq!((f.full_w, f.full_h), (640, 480));
                assert_eq!(f.rgba.len(), f.w as usize * f.h as usize * 4);
            }
        }
        // un fichier entier n'est jamais marque partiel, et l'en-tete seul ne donne rien a afficher
        assert!(!decode_display(&fx("a.jpg"), Target::Full, usize::MAX).unwrap().partial);
        let p = dir.path().join("head.jpg");
        std::fs::write(&p, &whole[..20]).unwrap();
        let r = decode_display(&p, Target::Full, usize::MAX);
        assert!(r.is_err(), "{:?}", r.map(|f| (f.w, f.h, f.full_w, f.full_h, f.partial)));
        // erreur fatale : refusee meme pour l'affichage
        std::fs::write(&p, [0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0]).unwrap();
        assert!(decode_display(&p, Target::Full, usize::MAX).is_err());
    }

    /// Code stream JPEG XL reduit a son en-tete : signature, SizeHeader de `w` x `h`, metadonnees par defaut.
    fn jxl_header(w: u32, h: u32) -> Vec<u8> {
        let (mut bits, mut n) = (0u128, 0u32);
        let mut put = |v: u64, len: u32| {
            bits |= u128::from(v) << n;
            n += len;
        };
        put(0, 1); // div8
        put(3, 2); // ysize : 30 bits + 1
        put(u64::from(h - 1), 30);
        put(0, 3); // ratio 0 : xsize explicite
        put(3, 2);
        put(u64::from(w - 1), 30);
        put(1, 1); // ImageMetadata all_default
        put(1, 1); // default_m : matrice opsin par defaut
        let mut out = vec![0xFF, 0x0A];
        out.extend_from_slice(&bits.to_le_bytes()[..n.div_ceil(8) as usize]);
        out.extend_from_slice(&[0; 16]); // debut d'un frame, que le decodeur ne lira pas
        out
    }

    /// `g.heic` dont les boites `ispe` annoncent `w` x `h`.
    fn heic_claiming(w: u32, h: u32) -> Vec<u8> {
        let mut data = std::fs::read(fx("g.heic")).unwrap();
        let mut found = 0;
        for at in (0..data.len() - 16).filter(|&i| &data[i..i + 4] == b"ispe").collect::<Vec<_>>() {
            data[at + 8..at + 12].copy_from_slice(&w.to_be_bytes());
            data[at + 12..at + 16].copy_from_slice(&h.to_be_bytes());
            found += 1;
        }
        assert!(found > 0);
        data
    }

    #[test]
    fn huge_headers_are_refused_before_any_allocation() {
        let dir = crate::testutil::TmpDir::new("decode-huge");
        let msg = |r: Result<Frame>| match r {
            Err(Error::Decode(m)) => m,
            other => panic!("{:?}", other.map(|f| (f.w, f.h))),
        };
        // 20000 x 20000 : 400 Mpx, rien n'est alloue (le fichier ne fait que quelques octets de donnees)
        let jxl = dir.path().join("huge.jxl");
        std::fs::write(&jxl, jxl_header(20000, 20000)).unwrap();
        for target in [Target::Preview, Target::Full] {
            let m = msg(decode(&jxl, target));
            assert!(m.contains("plafond"), "jxl {target:?} : {m}");
        }
        let heic = dir.path().join("huge.heic");
        std::fs::write(&heic, heic_claiming(20000, 20000)).unwrap();
        for target in [Target::Preview, Target::Full] {
            assert!(msg(decode(&heic, target)).contains("plafond"), "heic {target:?}");
        }
        // pile au plafond : accepte par check_dims (le decodage echoue ensuite sur le flux, pas sur les dimensions)
        assert!(check_dims(20000, 10000).is_ok() && check_dims(20000, 10001).is_err());
        assert!(check_dims(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn corrupt_and_missing_files_are_errors_not_panics() {
        let dir = crate::testutil::TmpDir::new("decode-corrupt");
        let missing = decode(&dir.path().join("absent.jpg"), Target::Full);
        assert!(missing.is_err());
        for (name, head) in [
            ("bad.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0][..]),
            ("bad.png", &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0][..]),
            ("bad.heic", &b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"[..]),
            ("bad.jxl", &[0xFF, 0x0A, 1, 2, 3][..]),
        ] {
            let p = dir.path().join(name);
            std::fs::write(&p, head).unwrap();
            assert!(decode(&p, Target::Full).is_err(), "{name}");
        }
    }
}
