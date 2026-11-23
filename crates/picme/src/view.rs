//! Geometrie de la vue : fit, zoom, pan, rotation par quarts de tour. Calcul pur en device pixels, sans GTK.
//!
//! Au fit (`zoom` absent) le rectangle est celui de `fit_dims`, a l'entier pres, centre : une texture `Fit` decodee
//! pour la vue tombe pixel pour pixel, sans filtrage. Zoome, l'echelle est en pixels ecran par pixel image (1.0 =
//! 100 %) et l'origine est arrondie au device pixel, pour qu'un 100 % ne soit pas flou. Une echelle qui ne depasse
//! plus celle du fit (fenetre agrandie, image tournee) vaut le fit.

use crate::app::fit_dims;

/// Echelle maximale (1600 %).
pub const MAX_ZOOM: f64 = 16.0;
/// Facteur d'un cran de molette.
pub const ZOOM_STEP: f64 = 1.25;
/// Une echelle a moins de ce rapport du fit est le fit.
const FIT_EPS: f64 = 1.0005;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct View {
    zoom: Option<f64>,
    /// Decalage du centre de l'image par rapport au centre de la vue, en device pixels.
    pan: (f64, f64),
    /// Quarts de tour dans le sens horaire, 0 a 3.
    quarters: u8,
}

/// Dimensions `full` (l'image orientee) vues apres `quarters` quarts de tour.
fn rotated(full: (u32, u32), quarters: u8) -> (u32, u32) {
    if quarters % 2 == 1 { (full.1, full.0) } else { full }
}

/// Decalage ramene la ou l'image couvre encore la vue : nul sur un axe ou elle tient.
fn clamp_pan(pan: (f64, f64), size: (f64, f64), vp: (f64, f64)) -> (f64, f64) {
    let axis = |p: f64, d: f64, v: f64| if d <= v { 0.0 } else { p.clamp(-(d - v) / 2.0, (d - v) / 2.0) };
    (axis(pan.0, size.0, vp.0), axis(pan.1, size.1, vp.1))
}

impl View {
    pub fn quarters(&self) -> u8 {
        self.quarters
    }

    /// Vrai si l'image est au fit (zoom absent, ou ne depassant pas le fit de la vue courante).
    pub fn is_fit(&self, full: (u32, u32), vp: (u32, u32)) -> bool {
        self.zoom.is_none_or(|z| z <= self.fit_scale(full, vp) * FIT_EPS)
    }

    /// Echelle du fit : l'image entiere dans la vue, jamais agrandie.
    pub fn fit_scale(&self, full: (u32, u32), vp: (u32, u32)) -> f64 {
        let (sw, sh) = rotated(full, self.quarters);
        (vp.0 as f64 / sw as f64).min(vp.1 as f64 / sh as f64).min(1.0)
    }

    /// Echelle affichee, en pixels ecran par pixel de l'image entiere.
    pub fn scale(&self, full: (u32, u32), vp: (u32, u32)) -> f64 {
        if self.is_fit(full, vp) { self.fit_scale(full, vp) } else { self.zoom.unwrap_or(1.0) }
    }

    pub fn percent(&self, full: (u32, u32), vp: (u32, u32)) -> u32 {
        (self.scale(full, vp) * 100.0).round() as u32
    }

    /// Rectangle de l'image tournee dans la vue, en device pixels.
    pub fn rect(&self, full: (u32, u32), vp: (u32, u32)) -> Rect {
        let (sw, sh) = rotated(full, self.quarters);
        if self.is_fit(full, vp) {
            let (dw, dh) = fit_dims(sw, sh, vp.0, vp.1);
            return Rect { x: (vp.0.saturating_sub(dw) / 2) as f64, y: (vp.1.saturating_sub(dh) / 2) as f64, w: dw as f64, h: dh as f64 };
        }
        let z = self.zoom.unwrap_or(1.0);
        let (w, h) = (sw as f64 * z, sh as f64 * z);
        let (vw, vh) = (vp.0 as f64, vp.1 as f64);
        let (px, py) = clamp_pan(self.pan, (w, h), (vw, vh));
        Rect { x: ((vw - w) / 2.0 + px).round(), y: ((vh - h) / 2.0 + py).round(), w, h }
    }

    /// Multiplie l'echelle par `factor` (borne entre le fit et `MAX_ZOOM`) en gardant fixe le point de l'image qui
    /// est sous `anchor` (device pixels, repere de la vue).
    pub fn zoom_by(&mut self, factor: f64, anchor: (f64, f64), full: (u32, u32), vp: (u32, u32)) {
        let z = self.scale(full, vp) * factor;
        self.zoom_to(z, anchor, full, vp);
    }

    /// Passe a l'echelle `z`, point sous `anchor` fixe. Au plus le fit : retour au fit.
    pub fn zoom_to(&mut self, z: f64, anchor: (f64, f64), full: (u32, u32), vp: (u32, u32)) {
        let fit = self.fit_scale(full, vp);
        let z = z.min(MAX_ZOOM);
        if z <= fit * FIT_EPS {
            self.zoom = None;
            self.pan = (0.0, 0.0);
            return;
        }
        let (z0, r0) = (self.scale(full, vp), self.rect(full, vp));
        // point de l'image sous l'ancre, en pixels de l'image tournee
        let (ix, iy) = ((anchor.0 - r0.x) / z0, (anchor.1 - r0.y) / z0);
        let (sw, sh) = rotated(full, self.quarters);
        let (w, h) = (sw as f64 * z, sh as f64 * z);
        let (vw, vh) = (vp.0 as f64, vp.1 as f64);
        let pan = (anchor.0 - ix * z - (vw - w) / 2.0, anchor.1 - iy * z - (vh - h) / 2.0);
        self.zoom = Some(z);
        self.pan = clamp_pan(pan, (w, h), (vw, vh));
    }

    /// Fit -> 100 % centre sur `anchor`, zoome -> fit. Rend `false` quand rien ne change (image qui tient a 100 %).
    pub fn toggle(&mut self, anchor: (f64, f64), full: (u32, u32), vp: (u32, u32)) -> bool {
        if !self.is_fit(full, vp) {
            self.zoom = None;
            self.pan = (0.0, 0.0);
            return true;
        }
        if self.fit_scale(full, vp) * FIT_EPS >= 1.0 {
            return false;
        }
        self.zoom_to(1.0, anchor, full, vp);
        true
    }

    /// Deplace l'image de `d` device pixels (glisser). Sans effet au fit.
    pub fn pan_by(&mut self, d: (f64, f64), full: (u32, u32), vp: (u32, u32)) {
        if self.is_fit(full, vp) {
            return;
        }
        let r = self.rect(full, vp);
        let (vw, vh) = (vp.0 as f64, vp.1 as f64);
        let pan = ((r.x + r.w / 2.0 - vw / 2.0) + d.0, (r.y + r.h / 2.0 - vh / 2.0) + d.1);
        self.pan = clamp_pan(pan, (r.w, r.h), (vw, vh));
    }

    /// Quarts de tour dans le sens horaire (negatif : anti-horaire). Le pan est remis au centre.
    pub fn rotate(&mut self, delta: i32) {
        self.quarters = (self.quarters as i32 + delta).rem_euclid(4) as u8;
        self.pan = (0.0, 0.0);
    }

    /// Retour au fit, sans rotation (autre image).
    pub fn reset(&mut self) {
        *self = View::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: (u32, u32) = (4000, 3000);
    const VP: (u32, u32) = (1000, 800);

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1.0
    }

    #[test]
    fn fit_is_the_exact_integer_rectangle_of_the_decoder() {
        let v = View::default();
        assert!(v.is_fit(FULL, VP));
        let (dw, dh) = fit_dims(FULL.0, FULL.1, VP.0, VP.1);
        assert_eq!((dw, dh), (1000, 750));
        assert_eq!(v.rect(FULL, VP), Rect { x: 0.0, y: 25.0, w: 1000.0, h: 750.0 });
        assert_eq!(v.percent(FULL, VP), 25);
        // une image plus petite que la vue n'est pas agrandie
        let r = v.rect((300, 200), VP);
        assert_eq!(r, Rect { x: 350.0, y: 300.0, w: 300.0, h: 200.0 });
        assert_eq!(v.percent((300, 200), VP), 100);
        // largeur impaire : centrage entier, jamais de demi-pixel
        assert_eq!(v.rect((301, 201), VP).x, 349.0);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let mut v = View::default();
        let anchor = (700.0, 300.0);
        let before = v.rect(FULL, VP);
        let z0 = v.scale(FULL, VP);
        let img = ((anchor.0 - before.x) / z0, (anchor.1 - before.y) / z0);
        v.zoom_by(ZOOM_STEP * ZOOM_STEP, anchor, FULL, VP);
        assert!(!v.is_fit(FULL, VP));
        let (z, r) = (v.scale(FULL, VP), v.rect(FULL, VP));
        assert!(near(z, z0 * ZOOM_STEP * ZOOM_STEP));
        assert!(near(r.x + img.0 * z, anchor.0) && near(r.y + img.1 * z, anchor.1), "{r:?}");
        // un autre cran, ancre ailleurs : meme invariant
        let anchor2 = (100.0, 700.0);
        let img2 = ((anchor2.0 - r.x) / z, (anchor2.1 - r.y) / z);
        v.zoom_by(ZOOM_STEP, anchor2, FULL, VP);
        let (z2, r2) = (v.scale(FULL, VP), v.rect(FULL, VP));
        assert!(near(r2.x + img2.0 * z2, anchor2.0) && near(r2.y + img2.1 * z2, anchor2.1), "{r2:?}");
    }

    #[test]
    fn zoom_is_bounded_by_the_fit_and_the_maximum() {
        let mut v = View::default();
        v.zoom_by(1.0 / ZOOM_STEP, (500.0, 400.0), FULL, VP);
        assert!(v.is_fit(FULL, VP), "pas en dessous du fit");
        for _ in 0..40 {
            v.zoom_by(ZOOM_STEP, (500.0, 400.0), FULL, VP);
        }
        assert_eq!(v.scale(FULL, VP), MAX_ZOOM);
        for _ in 0..40 {
            v.zoom_by(1.0 / ZOOM_STEP, (500.0, 400.0), FULL, VP);
        }
        assert!(v.is_fit(FULL, VP), "retour au fit en reculant");
        assert_eq!(v.rect(FULL, VP), View::default().rect(FULL, VP));
    }

    #[test]
    fn pan_never_leaves_the_image_and_a_hundred_percent_is_pixel_aligned() {
        let mut v = View::default();
        assert!(v.toggle((333.4, 271.9), FULL, VP));
        assert_eq!(v.scale(FULL, VP), 1.0);
        assert_eq!(v.percent(FULL, VP), 100);
        let r = v.rect(FULL, VP);
        assert_eq!((r.x.fract(), r.y.fract()), (0.0, 0.0));
        assert_eq!((r.w, r.h), (4000.0, 3000.0));

        v.pan_by((1e6, 1e6), FULL, VP);
        let r = v.rect(FULL, VP);
        assert_eq!((r.x, r.y), (0.0, 0.0), "bord haut-gauche contre le bord de la vue");
        v.pan_by((-1e6, -1e6), FULL, VP);
        let r = v.rect(FULL, VP);
        assert_eq!((r.x + r.w, r.y + r.h), (1000.0, 800.0), "bord bas-droite contre le bord de la vue");
        // le zoom depuis le coin garde l'image couvrante aussi
        v.zoom_by(0.9, (1000.0, 800.0), FULL, VP);
        let r = v.rect(FULL, VP);
        assert!(r.x <= 0.0 && r.y <= 0.0 && r.x + r.w >= 1000.0 && r.y + r.h >= 800.0, "{r:?}");
    }

    #[test]
    fn toggle_goes_back_to_fit_and_does_nothing_when_the_image_already_fits_at_100() {
        let mut v = View::default();
        assert!(v.toggle((500.0, 400.0), FULL, VP));
        assert!(v.toggle((500.0, 400.0), FULL, VP));
        assert!(v.is_fit(FULL, VP));
        assert!(!v.toggle((500.0, 400.0), (300, 200), VP), "100 % = fit");
        assert!(v.is_fit((300, 200), VP));
        // glisser au fit : rien
        v.pan_by((50.0, 50.0), FULL, VP);
        assert_eq!(v.rect(FULL, VP), View::default().rect(FULL, VP));
    }

    #[test]
    fn quarter_turns_swap_the_displayed_dimensions() {
        let mut v = View::default();
        v.rotate(1);
        assert_eq!(v.quarters(), 1);
        let r = v.rect(FULL, (1000, 1000));
        assert_eq!((r.w, r.h), (750.0, 1000.0), "4000x3000 tourne : 3000x4000 dans une vue carree");
        v.rotate(-2);
        assert_eq!(v.quarters(), 3);
        v.rotate(1);
        assert_eq!(v.quarters(), 0);
        assert_eq!(v.rect(FULL, (1000, 1000)).w, 1000.0);
        v.rotate(5);
        assert_eq!(v.quarters(), 1);
    }

    #[test]
    fn a_zoom_that_no_longer_exceeds_the_fit_is_the_fit() {
        let mut v = View::default();
        v.zoom_to(0.4, (500.0, 400.0), FULL, VP);
        assert!(!v.is_fit(FULL, VP));
        // fenetre agrandie : le fit passe a 0.5, l'echelle 0.4 n'est plus un zoom
        let big = (2000, 1600);
        assert!(v.is_fit(FULL, big));
        assert_eq!(v.rect(FULL, big), View::default().rect(FULL, big));
        assert_eq!(v.percent(FULL, big), 50);
    }

    #[test]
    fn reset_forgets_zoom_and_rotation() {
        let mut v = View::default();
        v.rotate(1);
        v.toggle((10.0, 10.0), FULL, VP);
        v.reset();
        assert_eq!(v, View::default());
    }
}
