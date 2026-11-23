//! Orientation EXIF : les 8 valeurs comme groupe (compose, inverse), application a un buffer RGBA, conversion
//! d'un rectangle affiche vers le rectangle stocke.
//!
//! Une `Orientation` est la transformation qui amene l'image stockee a l'image affichee. En interne c'est un
//! element du groupe diedral : une symetrie horizontale optionnelle, puis `rot` quarts de tour horaires.

use std::io::{BufRead, Seek};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Orientation {
    /// EXIF 1.
    #[default]
    Normal,
    /// EXIF 2 : miroir gauche-droite.
    MirrorH,
    /// EXIF 3.
    Rotate180,
    /// EXIF 4 : miroir haut-bas.
    MirrorV,
    /// EXIF 5 : symetrie par la diagonale principale.
    Transpose,
    /// EXIF 6 : quart de tour horaire pour afficher.
    Rotate90,
    /// EXIF 7 : symetrie par l'anti-diagonale.
    Transverse,
    /// EXIF 8 : quart de tour anti-horaire pour afficher.
    Rotate270,
}

/// Rectangle en pixels, coin haut gauche et taille.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Orientation {
    pub const ALL: [Orientation; 8] = [
        Orientation::Normal,
        Orientation::MirrorH,
        Orientation::Rotate180,
        Orientation::MirrorV,
        Orientation::Transpose,
        Orientation::Rotate90,
        Orientation::Transverse,
        Orientation::Rotate270,
    ];

    /// Valeur du tag EXIF 0x0112 (1 a 8). `None` hors de cette plage.
    pub fn from_exif(v: u32) -> Option<Orientation> {
        Self::ALL.get(usize::try_from(v).ok()?.checked_sub(1)?).copied()
    }

    pub fn exif_value(self) -> u16 {
        Self::ALL.iter().position(|o| *o == self).unwrap() as u16 + 1
    }

    /// (symetrie horizontale d'abord, quarts de tour horaires ensuite).
    fn parts(self) -> (bool, u8) {
        match self {
            Orientation::Normal => (false, 0),
            Orientation::MirrorH => (true, 0),
            Orientation::Rotate180 => (false, 2),
            Orientation::MirrorV => (true, 2),
            Orientation::Transpose => (true, 3),
            Orientation::Rotate90 => (false, 1),
            Orientation::Transverse => (true, 1),
            Orientation::Rotate270 => (false, 3),
        }
    }

    fn from_parts(flip: bool, rot: u8) -> Orientation {
        match (flip, rot % 4) {
            (false, 0) => Orientation::Normal,
            (true, 0) => Orientation::MirrorH,
            (false, 2) => Orientation::Rotate180,
            (true, 2) => Orientation::MirrorV,
            (true, 3) => Orientation::Transpose,
            (false, 1) => Orientation::Rotate90,
            (true, 1) => Orientation::Transverse,
            _ => Orientation::Rotate270,
        }
    }

    /// Vrai si largeur et hauteur s'echangent a l'affichage (EXIF 5 a 8).
    pub fn swaps_axes(self) -> bool {
        self.parts().1 % 2 == 1
    }

    /// Dimensions affichees d'une image stockee en `w` x `h`.
    pub fn displayed_dims(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_axes() { (h, w) } else { (w, h) }
    }

    /// `self` puis `then` : appliquer `a.compose(b)` equivaut a appliquer `a`, puis `b`.
    /// C'est la nouvelle valeur du tag quand l'utilisateur ajoute la rotation `then` a une image deja orientee `self`.
    pub fn compose(self, then: Orientation) -> Orientation {
        let ((fa, ra), (fb, rb)) = (self.parts(), then.parts());
        // R^rb F^fb R^ra F^fa, avec F R^k = R^-k F
        let rot = if fb { rb + 4 - ra } else { rb + ra };
        Self::from_parts(fa ^ fb, rot % 4)
    }

    /// Transformation inverse : `o.compose(o.inverse())` est `Normal`.
    pub fn inverse(self) -> Orientation {
        let (flip, rot) = self.parts();
        // une symetrie est son propre inverse ; une rotation pure s'inverse en tournant dans l'autre sens
        if flip { self } else { Self::from_parts(false, (4 - rot) % 4) }
    }

    /// Position affichee du pixel (`x`, `y`) d'une image stockee en `w` x `h`, en signe (hors image accepte).
    fn map_point_i(self, x: i64, y: i64, w: i64, h: i64) -> (i64, i64) {
        let (flip, rot) = self.parts();
        let (x, y) = if flip { (w - 1 - x, y) } else { (x, y) };
        match rot {
            0 => (x, y),
            1 => (h - 1 - y, x),
            2 => (w - 1 - x, h - 1 - y),
            _ => (y, w - 1 - x),
        }
    }

    /// Position affichee du pixel (`x`, `y`) d'une image stockee en `w` x `h`.
    pub fn map_point(self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32) {
        let (dx, dy) = self.map_point_i(x.into(), y.into(), w.into(), h.into());
        (dx as u32, dy as u32)
    }

    /// Applique l'orientation a un buffer RGBA8 stocke en `w` x `h`. Rend le buffer affiche et ses dimensions
    /// (le buffer d'origine est rendu tel quel pour `Normal`).
    pub fn apply_rgba(self, buf: Vec<u8>, w: u32, h: u32) -> (Vec<u8>, u32, u32) {
        self.apply_px::<4>(buf, w, h)
    }

    /// [`apply_rgba`](Self::apply_rgba) pour des pixels de `N` octets, sans lignes bourrees (PNG brut : 1 a 8 octets).
    pub fn apply_px<const N: usize>(self, buf: Vec<u8>, w: u32, h: u32) -> (Vec<u8>, u32, u32) {
        assert_eq!(buf.len(), w as usize * h as usize * N, "buffer de taille incoherente");
        if self == Orientation::Normal {
            return (buf, w, h);
        }
        let (dw, dh) = self.displayed_dims(w, h);
        // Pour chaque pixel affiche on lit le pixel stocke : la correspondance est affine, donc un depart et deux pas.
        let inv = self.inverse();
        let (iw, ih, sw) = (i64::from(dw), i64::from(dh), i64::from(w));
        let origin = inv.map_point_i(0, 0, iw, ih);
        let along_x = inv.map_point_i(1, 0, iw, ih);
        let along_y = inv.map_point_i(0, 1, iw, ih);
        let start = origin.1 * sw + origin.0;
        let step_x = (along_x.1 - origin.1) * sw + (along_x.0 - origin.0);
        let step_y = (along_y.1 - origin.1) * sw + (along_y.0 - origin.0);

        let mut out = vec![0u8; buf.len()];
        for (dy, row) in out.chunks_exact_mut(dw as usize * N).enumerate() {
            let mut idx = start + dy as i64 * step_y;
            for px in row.as_chunks_mut::<N>().0 {
                let s = idx as usize * N;
                px.copy_from_slice(&buf[s..s + N]);
                idx += step_x;
            }
        }
        (out, dw, dh)
    }

    /// Rectangle affiche (`r`, en pixels de l'image affichee) -> rectangle de l'image stockee en `w` x `h`.
    /// Le crop se fait sur le stocke, l'utilisateur dessine sur l'affiche.
    pub fn map_rect(self, r: Rect, w: u32, h: u32) -> Rect {
        let (dw, dh) = self.displayed_dims(w, h);
        assert!(r.w > 0 && r.h > 0 && r.x + r.w <= dw && r.y + r.h <= dh, "rectangle hors de l'image affichee");
        let inv = self.inverse();
        // deux coins opposes, pixels inclus ; l'inverse mappe l'affiche vers le stocke
        let a = inv.map_point(r.x, r.y, dw, dh);
        let b = inv.map_point(r.x + r.w - 1, r.y + r.h - 1, dw, dh);
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 }
    }

    /// Orientation lue dans le conteneur (JPEG, TIFF, PNG, WebP). `Normal` si absente ou illisible.
    pub fn from_container<R: BufRead + Seek>(reader: &mut R) -> Orientation {
        let Ok(exif) = exif::Reader::new().read_from_container(reader) else {
            return Orientation::Normal;
        };
        exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .and_then(Orientation::from_exif)
            .unwrap_or_default()
    }

    /// Orientation EXIF d'un fichier ; `Normal` si absente ou illisible.
    pub fn read(path: &Path) -> Orientation {
        match std::fs::File::open(path) {
            Ok(f) => Self::from_container(&mut std::io::BufReader::new(f)),
            Err(_) => Orientation::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures_dir;

    /// Image de test : pixel (x, y) vaut `y * w + x + 1` dans son premier octet.
    fn image(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| [(i + 1) as u8, 0, 0, 255]).collect()
    }

    fn ids(buf: &[u8]) -> Vec<u8> {
        buf.as_chunks::<4>().0.iter().map(|p| p[0]).collect()
    }

    #[test]
    fn exif_values_round_trip() {
        for (i, o) in Orientation::ALL.iter().enumerate() {
            assert_eq!(o.exif_value() as usize, i + 1);
            assert_eq!(Orientation::from_exif(i as u32 + 1), Some(*o));
        }
        assert_eq!(Orientation::from_exif(0), None);
        assert_eq!(Orientation::from_exif(9), None);
    }

    /// Resultats attendus ecrits a la main sur 1 2 3 / 4 5 6 (3 x 2), d'apres la definition du tag EXIF.
    #[test]
    fn apply_matches_hand_written_tables() {
        let expected: [(Orientation, (u32, u32), [u8; 6]); 8] = [
            (Orientation::Normal, (3, 2), [1, 2, 3, 4, 5, 6]),
            (Orientation::MirrorH, (3, 2), [3, 2, 1, 6, 5, 4]),
            (Orientation::Rotate180, (3, 2), [6, 5, 4, 3, 2, 1]),
            (Orientation::MirrorV, (3, 2), [4, 5, 6, 1, 2, 3]),
            (Orientation::Transpose, (2, 3), [1, 4, 2, 5, 3, 6]),
            (Orientation::Rotate90, (2, 3), [4, 1, 5, 2, 6, 3]),
            (Orientation::Transverse, (2, 3), [6, 3, 5, 2, 4, 1]),
            (Orientation::Rotate270, (2, 3), [3, 6, 2, 5, 1, 4]),
        ];
        for (o, dims, pixels) in expected {
            let (out, w, h) = o.apply_rgba(image(3, 2), 3, 2);
            assert_eq!((w, h), dims, "{o:?}");
            assert_eq!(ids(&out), pixels, "{o:?}");
            assert_eq!(o.displayed_dims(3, 2), dims);
        }
    }

    /// Les 64 couples : appliquer a puis b donne la meme image qu'appliquer a.compose(b).
    #[test]
    fn compose_matches_successive_application() {
        for a in Orientation::ALL {
            for b in Orientation::ALL {
                let (ia, wa, ha) = a.apply_rgba(image(3, 2), 3, 2);
                let (successive, w, h) = b.apply_rgba(ia, wa, ha);
                let (direct, dw, dh) = a.compose(b).apply_rgba(image(3, 2), 3, 2);
                assert_eq!((w, h), (dw, dh), "{a:?} puis {b:?}");
                assert_eq!(successive, direct, "{a:?} puis {b:?}");
            }
        }
    }

    #[test]
    fn compose_is_a_group() {
        for a in Orientation::ALL {
            assert_eq!(a.compose(Orientation::Normal), a);
            assert_eq!(Orientation::Normal.compose(a), a);
            assert_eq!(a.compose(a.inverse()), Orientation::Normal, "{a:?}");
            assert_eq!(a.inverse().compose(a), Orientation::Normal, "{a:?}");
            for b in Orientation::ALL {
                for c in Orientation::ALL {
                    assert_eq!(a.compose(b).compose(c), a.compose(b.compose(c)));
                }
            }
        }
        // quatre quarts de tour : identite ; un quart de tour sur EXIF 6 donne EXIF 3
        let r = Orientation::Rotate90;
        assert_eq!(r.compose(r).compose(r).compose(r), Orientation::Normal);
        assert_eq!(r.compose(r), Orientation::Rotate180);
    }

    #[test]
    fn inverse_undoes_apply() {
        for o in Orientation::ALL {
            let (shown, w, h) = o.apply_rgba(image(5, 3), 5, 3);
            let (back, bw, bh) = o.inverse().apply_rgba(shown, w, h);
            assert_eq!((bw, bh), (5, 3), "{o:?}");
            assert_eq!(back, image(5, 3), "{o:?}");
        }
    }

    #[test]
    fn apply_handles_one_pixel_wide_images() {
        for o in Orientation::ALL {
            let (out, w, h) = o.apply_rgba(image(1, 3), 1, 3);
            assert_eq!(w as usize * h as usize, 3);
            let mut got = ids(&out);
            got.sort();
            assert_eq!(got, [1, 2, 3], "{o:?}");
        }
    }

    fn crop(buf: &[u8], w: u32, r: Rect) -> Vec<u8> {
        let mut out = Vec::new();
        for y in r.y..r.y + r.h {
            let s = (y * w + r.x) as usize * 4;
            out.extend_from_slice(&buf[s..s + r.w as usize * 4]);
        }
        out
    }

    /// Cropper l'image affichee equivaut a cropper le stocke au rectangle converti, puis a orienter.
    #[test]
    fn map_rect_crops_the_same_pixels() {
        let (w, h) = (7u32, 5u32);
        let stored = image(w, h);
        for o in Orientation::ALL {
            let (shown, dw, dh) = o.apply_rgba(stored.clone(), w, h);
            for r in [
                Rect { x: 0, y: 0, w: dw, h: dh },
                Rect { x: 0, y: 0, w: 1, h: 1 },
                Rect { x: dw - 1, y: dh - 1, w: 1, h: 1 },
                Rect { x: 1, y: 2, w: 3, h: 2 },
                Rect { x: 2, y: 0, w: 1, h: dh },
                Rect { x: 0, y: 1, w: dw, h: 2 },
            ] {
                let want = crop(&shown, dw, r);
                let m = o.map_rect(r, w, h);
                assert!(m.x + m.w <= w && m.y + m.h <= h, "{o:?} {r:?} -> {m:?}");
                let cropped = crop(&stored, w, m);
                let (got, gw, gh) = o.apply_rgba(cropped, m.w, m.h);
                assert_eq!((gw, gh), (r.w, r.h), "{o:?} {r:?}");
                assert_eq!(got, want, "{o:?} {r:?}");
            }
        }
    }

    #[test]
    fn map_rect_on_the_rotated_corner() {
        // EXIF 6, 640x480 stocke : le coin haut gauche stocke est en haut a droite de l'affiche (480x640)
        let stored = Orientation::Rotate90.map_rect(Rect { x: 480 - 80, y: 0, w: 80, h: 120 }, 640, 480);
        assert_eq!(stored, Rect { x: 0, y: 0, w: 120, h: 80 });
    }

    #[test]
    fn reads_the_orientation_tag_of_fixtures() {
        assert_eq!(Orientation::read(&fixtures_dir().join("c_orient6.jpg")), Orientation::Rotate90);
        assert_eq!(Orientation::read(&fixtures_dir().join("a.jpg")), Orientation::Normal);
        assert_eq!(Orientation::read(&fixtures_dir().join("d.png")), Orientation::Normal);
        assert_eq!(Orientation::read(&fixtures_dir().join("absent.jpg")), Orientation::Normal);
    }
}
