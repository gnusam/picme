//! Decode vers RGBA8, orientation deja appliquee.
//!
//! Chaque format a son module ; tous passent par le meme plan de sortie (`Plan`) : les dimensions cibles se
//! calculent dans l'espace affiche, puis se convertissent dans l'espace stocke pour reduire AVANT d'orienter
//! (la rotation travaille sur moins de pixels). Aucun `catch_unwind` ici : c'est a l'appelant d'envelopper.

mod heif;
mod jpeg;
mod jxl;
mod raster;
mod resize;

use crate::error::{Error, Result};
use crate::media::{self, MediaKind};
use crate::orient::Orientation;
use std::path::Path;

/// Cote de la boite du `Preview` : celle de la miniature freedesktop `large`.
pub const PREVIEW_SIZE: u32 = 256;

/// Budget RAM par defaut du cache de textures (`--cache-mb`, 256 Mo). `Full` est reduit au-dela de la moitie.
pub const DEFAULT_BUDGET_BYTES: usize = 256 << 20;

/// Taille voulue. Dans les trois cas l'image garde ses proportions et n'est jamais agrandie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Premier affichage, le plus vite possible : tient dans `PREVIEW_SIZE` x `PREVIEW_SIZE`. Pour un HEIF,
    /// la thumbnail embarquee si elle existe, donc eventuellement plus petite que la boite (voir `Frame::w`).
    /// Une miniature freedesktop `large` doit venir de `Fit(256, 256)`, pas de `Preview`.
    Preview,
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
}

impl Frame {
    /// Octets du buffer RGBA : ce que le cache LRU compte.
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// Decode `path` a la taille `target`, avec le plafond de `Full` du budget par defaut.
pub fn decode(path: &Path, target: Target) -> Result<Frame> {
    decode_capped(path, target, DEFAULT_BUDGET_BYTES / 2)
}

/// Comme `decode`, avec un plafond explicite (en octets RGBA) pour `Full` : au-dela, l'image est reduite de
/// facon a tenir dans `max_full_bytes`. Le plafond vaut la moitie du budget du cache (`--cache-mb`).
pub fn decode_capped(path: &Path, target: Target, max_full_bytes: usize) -> Result<Frame> {
    if let Target::Fit(0, _) | Target::Fit(_, 0) = target {
        return Err(Error::Invalid("Fit demande une fenetre vide".into()));
    }
    match media::detect(path) {
        Some(MediaKind::Jpeg) => jpeg::decode(path, target, max_full_bytes),
        Some(kind @ (MediaKind::Png | MediaKind::Webp | MediaKind::Tiff)) => raster::decode(path, kind, target, max_full_bytes),
        Some(MediaKind::Heif) => heif::decode(path, target, max_full_bytes),
        Some(MediaKind::Jxl) => jxl::decode(path, target, max_full_bytes),
        Some(MediaKind::Raw) => Err(Error::Unsupported("RAW".into())),
        Some(MediaKind::Video) => Err(Error::Unsupported("video".into())),
        None => Err(Error::Unsupported(format!("type inconnu : {}", path.display()))),
    }
}

/// Dimensions de sortie, dans l'espace affiche, pour une image entiere de `fw` x `fh`.
fn out_dims(fw: u32, fh: u32, target: Target, max_full_bytes: usize) -> (u32, u32) {
    let (bw, bh) = match target {
        Target::Preview => (PREVIEW_SIZE, PREVIEW_SIZE),
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
        Ok(Frame { rgba, w, h, full_w: self.full_w, full_h: self.full_h })
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
        let reduced = Frame { rgba: reduced, w: 80, h: 60, full_w: 640, full_h: 480 };
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
    fn refuses_video_raw_and_unknown() {
        assert!(matches!(decode(&fx("j.mov"), Target::Full), Err(Error::Unsupported(_))));
        let dir = crate::testutil::TmpDir::new("decode-unsupported");
        let txt = dir.path().join("a.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(matches!(decode(&txt, Target::Full), Err(Error::Unsupported(_))));
        assert!(matches!(decode(&fx("a.jpg"), Target::Fit(0, 10)), Err(Error::Invalid(_))));
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
