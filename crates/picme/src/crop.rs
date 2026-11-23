//! Cadre de recadrage (touche C) : un rectangle libre dessine par-dessus l'image au fit, hors du rectangle l'image est
//! assombrie. La geometrie (poignees, deplacement, redimensionnement) est dans `CropRect`, sans GTK, en fractions de
//! l'image affichee comme `edit::crop::Frac` ; le dessin et les gestes sont dans `Cropper`.

use gtk4 as gtk;
use gtk::{gdk, prelude::*};
use picme_core::edit::crop::Frac;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Cote minimal du cadre, en pixels de l'ecran.
const MIN_PX: f64 = 24.0;
/// Rayon de prise des poignees, en pixels de l'ecran.
const GRAB_PX: f64 = 12.0;
/// Cote du carre d'une poignee, en pixels de l'ecran.
const HANDLE_PX: f64 = 8.0;
/// Cadre de depart : l'image moins cette marge de chaque cote, en fraction.
const START_MARGIN: f64 = 0.1;

/// L'image sur l'ecran, en pixels logiques de l'overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Ce que le pointeur tient : un bord, un coin ou tout le cadre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grip {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Grip {
    /// Nom CSS du curseur.
    pub fn cursor(self) -> &'static str {
        match self {
            Grip::Move => "move",
            Grip::N => "n-resize",
            Grip::S => "s-resize",
            Grip::E => "e-resize",
            Grip::W => "w-resize",
            Grip::NE => "ne-resize",
            Grip::NW => "nw-resize",
            Grip::SE => "se-resize",
            Grip::SW => "sw-resize",
        }
    }
}

/// Le cadre : ses quatre bords, en fractions de l'image (0 a 1, origine en haut a gauche).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl CropRect {
    /// Cadre de depart, centre.
    pub fn start() -> Self {
        CropRect { x0: START_MARGIN, y0: START_MARGIN, x1: 1.0 - START_MARGIN, y1: 1.0 - START_MARGIN }
    }

    pub fn frac(&self) -> Frac {
        Frac { x: self.x0, y: self.y0, w: self.x1 - self.x0, h: self.y1 - self.y0 }
    }

    /// Ce que tient le pointeur en `p` (pixels de l'overlay) : un coin, puis un bord, puis l'interieur. `None` hors du
    /// cadre et de la zone de prise. Pres d'un bord d'un cadre tres petit, les coins passent avant les bords.
    pub fn hit(&self, p: (f64, f64), area: Area) -> Option<Grip> {
        let (l, t) = (area.x + self.x0 * area.w, area.y + self.y0 * area.h);
        let (r, b) = (area.x + self.x1 * area.w, area.y + self.y1 * area.h);
        let near = |v: f64, edge: f64| (v - edge).abs() <= GRAB_PX;
        let inside_x = p.0 >= l - GRAB_PX && p.0 <= r + GRAB_PX;
        let inside_y = p.1 >= t - GRAB_PX && p.1 <= b + GRAB_PX;
        if !inside_x || !inside_y {
            return None;
        }
        // sur un cadre plus petit que deux zones de prise, le milieu garde le deplacement : on ne prend un bord que
        // du cote le plus proche
        let west = near(p.0, l) && (p.0 - l).abs() <= (p.0 - r).abs();
        let east = near(p.0, r) && !west;
        let north = near(p.1, t) && (p.1 - t).abs() <= (p.1 - b).abs();
        let south = near(p.1, b) && !north;
        Some(match (west, east, north, south) {
            (true, _, true, _) => Grip::NW,
            (_, true, true, _) => Grip::NE,
            (true, _, _, true) => Grip::SW,
            (_, true, _, true) => Grip::SE,
            (true, ..) => Grip::W,
            (_, true, ..) => Grip::E,
            (_, _, true, _) => Grip::N,
            (.., true) => Grip::S,
            _ => Grip::Move,
        })
    }

    /// Le cadre `start` tire de `d` (fractions de l'image) par `grip` : un bord ne depasse ni l'image ni le bord
    /// oppose moins la taille minimale ; deplacer garde la taille et s'arrete aux bords de l'image.
    pub fn dragged(start: CropRect, grip: Grip, d: (f64, f64), min: (f64, f64)) -> CropRect {
        let mut r = start;
        if grip == Grip::Move {
            let (w, h) = (start.x1 - start.x0, start.y1 - start.y0);
            r.x0 = (start.x0 + d.0).min(1.0 - w).max(0.0);
            r.y0 = (start.y0 + d.1).min(1.0 - h).max(0.0);
            r.x1 = r.x0 + w;
            r.y1 = r.y0 + h;
            return r;
        }
        // min/max et pas clamp : `min` grandit si la fenetre se reduit pendant le glisser, et clamp paniquerait
        if matches!(grip, Grip::W | Grip::NW | Grip::SW) {
            r.x0 = (start.x0 + d.0).min(start.x1 - min.0).max(0.0);
        }
        if matches!(grip, Grip::E | Grip::NE | Grip::SE) {
            r.x1 = (start.x1 + d.0).max(start.x0 + min.0).min(1.0);
        }
        if matches!(grip, Grip::N | Grip::NW | Grip::NE) {
            r.y0 = (start.y0 + d.1).min(start.y1 - min.1).max(0.0);
        }
        if matches!(grip, Grip::S | Grip::SW | Grip::SE) {
            r.y1 = (start.y1 + d.1).max(start.y0 + min.1).min(1.0);
        }
        r
    }

    /// Nouveau cadre minimal dont le coin haut gauche est en `p` (fractions) : le glisser qui suit tire son coin bas
    /// droit.
    pub fn begun(p: (f64, f64), min: (f64, f64)) -> CropRect {
        let (x0, y0) = (p.0.min(1.0 - min.0).max(0.0), p.1.min(1.0 - min.1).max(0.0));
        CropRect { x0, y0, x1: x0 + min.0, y1: y0 + min.1 }
    }
}

/// Cote minimal en fractions de l'image affichee.
fn min_frac(area: Area) -> (f64, f64) {
    ((MIN_PX / area.w.max(1.0)).min(1.0), (MIN_PX / area.h.max(1.0)).min(1.0))
}

type AreaOf = Box<dyn Fn() -> Option<Area>>;

/// Le widget : une zone de dessin par-dessus le canvas, cachee tant que le crop n'est pas ouvert.
pub struct Cropper {
    widget: gtk::DrawingArea,
    area_of: AreaOf,
    rect: Cell<CropRect>,
    /// Poignee tenue par le glisser en cours, avec le cadre au moment de la prise.
    held: RefCell<Option<(Grip, CropRect)>>,
}

impl Cropper {
    /// `area_of` rend la place de l'image sur l'ecran, `None` si elle n'est pas affichee a plat.
    pub fn new(area_of: impl Fn() -> Option<Area> + 'static) -> Rc<Cropper> {
        let widget = gtk::DrawingArea::new();
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        widget.set_visible(false);
        let c = Rc::new(Cropper { widget, area_of: Box::new(area_of), rect: Cell::new(CropRect::start()), held: RefCell::default() });
        c.connect();
        c
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.widget
    }

    pub fn is_active(&self) -> bool {
        self.widget.is_visible()
    }

    /// Ouvre le cadre de depart sur l'image affichee.
    pub fn start(&self) {
        self.rect.set(CropRect::start());
        self.held.take();
        self.widget.set_cursor(None);
        self.widget.set_visible(true);
        self.widget.queue_draw();
    }

    pub fn stop(&self) {
        self.held.take();
        self.widget.set_visible(false);
    }

    pub fn frac(&self) -> Frac {
        self.rect.get().frac()
    }

    fn connect(self: &Rc<Self>) {
        self.widget.set_draw_func({
            let c = Rc::downgrade(self);
            move |_, cr, _, _| {
                if let Some(c) = c.upgrade() {
                    c.draw(cr);
                }
            }
        });

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion({
            let c = Rc::downgrade(self);
            move |_, x, y| {
                let Some(c) = c.upgrade() else { return };
                // pendant un glisser le curseur reste celui de la prise
                if c.held.borrow().is_some() {
                    return;
                }
                let grip = (c.area_of)().and_then(|a| c.rect.get().hit((x, y), a));
                c.widget.set_cursor_from_name(Some(grip.map_or("crosshair", Grip::cursor)));
            }
        });
        self.widget.add_controller(motion);

        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        drag.connect_drag_begin({
            let c = Rc::downgrade(self);
            move |_, x, y| {
                let Some(c) = c.upgrade() else { return };
                let Some(area) = (c.area_of)() else { return };
                let rect = c.rect.get();
                let held = match rect.hit((x, y), area) {
                    Some(grip) => Some((grip, rect)),
                    // hors du cadre mais sur l'image : un nouveau cadre part de ce point
                    None if x >= area.x && x <= area.x + area.w && y >= area.y && y <= area.y + area.h => {
                        let fresh = CropRect::begun(((x - area.x) / area.w, (y - area.y) / area.h), min_frac(area));
                        c.rect.set(fresh);
                        Some((Grip::SE, fresh))
                    }
                    None => None,
                };
                if let Some((grip, _)) = held {
                    c.widget.set_cursor_from_name(Some(grip.cursor()));
                }
                *c.held.borrow_mut() = held;
            }
        });
        drag.connect_drag_update({
            let c = Rc::downgrade(self);
            move |_, ox, oy| {
                let Some(c) = c.upgrade() else { return };
                let (Some((grip, start)), Some(area)) = (*c.held.borrow(), (c.area_of)()) else { return };
                c.rect.set(CropRect::dragged(start, grip, (ox / area.w.max(1.0), oy / area.h.max(1.0)), min_frac(area)));
                c.widget.queue_draw();
            }
        });
        drag.connect_drag_end({
            let c = Rc::downgrade(self);
            move |_, _, _| {
                if let Some(c) = c.upgrade() {
                    c.held.take();
                }
            }
        });
        self.widget.add_controller(drag);
    }

    fn draw(&self, cr: &gtk::cairo::Context) {
        let Some(a) = (self.area_of)() else { return };
        let r = self.rect.get();
        let (l, t) = (a.x + r.x0 * a.w, a.y + r.y0 * a.h);
        let (rw, rh) = ((r.x1 - r.x0) * a.w, (r.y1 - r.y0) * a.h);
        // l'image hors du cadre, assombrie (le trou est le cadre : regle pair-impair)
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        cr.rectangle(a.x, a.y, a.w, a.h);
        cr.rectangle(l, t, rw, rh);
        let _ = cr.fill();
        // fin liseré blanc : un pixel entier pour rester net
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.set_line_width(1.0);
        cr.rectangle(l.round() + 0.5, t.round() + 0.5, rw.round(), rh.round());
        let _ = cr.stroke();
        // poignees : les quatre coins et les milieux des bords
        let (mx, my) = (l + rw / 2.0, t + rh / 2.0);
        for (x, y) in [(l, t), (mx, t), (l + rw, t), (l, my), (l + rw, my), (l, t + rh), (mx, t + rh), (l + rw, t + rh)] {
            let hs = HANDLE_PX;
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
            cr.rectangle((x - hs / 2.0).round() - 1.0, (y - hs / 2.0).round() - 1.0, hs + 2.0, hs + 2.0);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.rectangle((x - hs / 2.0).round(), (y - hs / 2.0).round(), hs, hs);
            let _ = cr.fill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Area = Area { x: 100.0, y: 50.0, w: 800.0, h: 400.0 };
    const MIN: (f64, f64) = (0.03, 0.06);

    fn sq(x0: f64, y0: f64, x1: f64, y1: f64) -> CropRect {
        CropRect { x0, y0, x1, y1 }
    }

    #[test]
    fn the_start_frame_is_centered_and_converts_to_fractions() {
        let f = CropRect::start().frac();
        assert!((f.x - 0.1).abs() < 1e-9 && (f.y - 0.1).abs() < 1e-9 && (f.w - 0.8).abs() < 1e-9 && (f.h - 0.8).abs() < 1e-9);
    }

    #[test]
    fn hit_finds_corners_edges_inside_and_nothing_far_away() {
        let r = sq(0.25, 0.25, 0.75, 0.75);
        // cadre a l'ecran : x 300..700, y 150..350
        assert_eq!(r.hit((300.0, 150.0), AREA), Some(Grip::NW));
        assert_eq!(r.hit((702.0, 349.0), AREA), Some(Grip::SE));
        assert_eq!(r.hit((700.0, 150.0), AREA), Some(Grip::NE));
        assert_eq!(r.hit((300.0, 350.0), AREA), Some(Grip::SW));
        assert_eq!(r.hit((500.0, 148.0), AREA), Some(Grip::N));
        assert_eq!(r.hit((500.0, 352.0), AREA), Some(Grip::S));
        assert_eq!(r.hit((296.0, 250.0), AREA), Some(Grip::W));
        assert_eq!(r.hit((703.0, 250.0), AREA), Some(Grip::E));
        assert_eq!(r.hit((500.0, 250.0), AREA), Some(Grip::Move));
        assert_eq!(r.hit((150.0, 100.0), AREA), None);
        assert_eq!(r.hit((500.0, 400.0), AREA), None);
    }

    #[test]
    fn dragging_an_edge_stops_at_the_image_and_at_the_minimum_size() {
        let r = sq(0.25, 0.25, 0.75, 0.75);
        assert_eq!(CropRect::dragged(r, Grip::W, (-0.5, 0.0), MIN).x0, 0.0);
        assert_eq!(CropRect::dragged(r, Grip::E, (0.5, 0.0), MIN).x1, 1.0);
        assert!((CropRect::dragged(r, Grip::W, (0.9, 0.0), MIN).x0 - (0.75 - 0.03)).abs() < 1e-9, "ne passe pas l'autre bord");
        assert!((CropRect::dragged(r, Grip::S, (0.0, -0.9), MIN).y1 - (0.25 + 0.06)).abs() < 1e-9);
        // un bord ne bouge que sur son axe
        let n = CropRect::dragged(r, Grip::N, (0.2, -0.1), MIN);
        assert_eq!((n.x0, n.x1, n.y1), (0.25, 0.75, 0.75));
        assert!((n.y0 - 0.15).abs() < 1e-9);
        // un coin bouge deux bords
        let c = CropRect::dragged(r, Grip::SE, (0.1, 0.1), MIN);
        assert!((c.x1 - 0.85).abs() < 1e-9 && (c.y1 - 0.85).abs() < 1e-9 && c.x0 == 0.25 && c.y0 == 0.25);
    }

    #[test]
    fn moving_keeps_the_size_and_stays_inside_the_image() {
        let r = sq(0.25, 0.25, 0.75, 0.75);
        let m = CropRect::dragged(r, Grip::Move, (0.1, -0.1), MIN);
        assert!((m.x0 - 0.35).abs() < 1e-9 && (m.x1 - 0.85).abs() < 1e-9 && (m.y0 - 0.15).abs() < 1e-9 && (m.y1 - 0.65).abs() < 1e-9);
        let far = CropRect::dragged(r, Grip::Move, (5.0, 5.0), MIN);
        assert_eq!((far.x1, far.y1), (1.0, 1.0));
        assert!((far.x1 - far.x0 - 0.5).abs() < 1e-9);
        let near = CropRect::dragged(r, Grip::Move, (-5.0, -5.0), MIN);
        assert_eq!((near.x0, near.y0), (0.0, 0.0));
    }

    #[test]
    fn a_new_frame_begins_at_the_point_and_fits_in_the_image() {
        let b = CropRect::begun((0.4, 0.5), MIN);
        assert_eq!((b.x0, b.y0), (0.4, 0.5));
        assert!((b.x1 - 0.43).abs() < 1e-9 && (b.y1 - 0.56).abs() < 1e-9);
        let edge = CropRect::begun((1.0, 1.0), MIN);
        assert!((edge.x1 - 1.0).abs() < 1e-9 && (edge.y1 - 1.0).abs() < 1e-9 && edge.x1 - edge.x0 >= MIN.0 - 1e-9);
    }

    #[test]
    fn the_minimum_follows_the_displayed_size() {
        let (mx, my) = min_frac(AREA);
        assert!((mx - 24.0 / 800.0).abs() < 1e-9 && (my - 24.0 / 400.0).abs() < 1e-9);
    }
}
