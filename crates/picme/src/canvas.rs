//! Vue de l'image : fit centre par defaut, zoom a la molette (Ctrl) ou au double-clic, glisser pour deplacer, rotation
//! visuelle par quarts de tour. La geometrie est dans `view` (device pixels, sans GTK) ; ici seuls vivent les gestes
//! et le dessin. La place a l'ecran vient des dimensions de l'image entiere, pas de la texture : un `Preview` de
//! 256 px occupe deja la place exacte du `Fit` qui le remplacera, sans saut ; au fit, le rectangle est a l'entier
//! de device pixel pres, la texture `Fit` est copiee sans filtrage.

use crate::view::{View, ZOOM_STEP};
use gtk4 as gtk;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, prelude::*};
use picme_core::decode::Frame;

type OnResize = Box<dyn Fn(u32, u32)>;
/// Appele apres chaque changement de la vue voulu par l'utilisateur, avec "au-dessus du fit".
type OnView = Box<dyn Fn(bool)>;
type OnNav = Box<dyn Fn(isize)>;

/// Texture d'une frame RGBA decodee (la copie est evitee : les octets sont donnes a GDK).
pub fn texture_of(f: Frame) -> gdk::Texture {
    let stride = f.w as usize * 4;
    gdk::MemoryTexture::new(f.w as i32, f.h as i32, gdk::MemoryFormat::R8g8b8a8, &glib::Bytes::from_owned(f.rgba), stride).upcast()
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct Canvas {
        /// Texture et dimensions de l'image entiere.
        pub image: RefCell<Option<(gdk::Texture, u32, u32)>>,
        pub view: Cell<View>,
        /// Dernier point survole, en device pixels : ancre du zoom au clavier.
        pub pointer: Cell<Option<(f64, f64)>>,
        /// La vue etait au-dessus du fit a la derniere notification.
        pub beyond: Cell<bool>,
        /// Deplacement deja applique du glisser en cours, en pixels logiques.
        pub drag_done: Cell<(f64, f64)>,
        /// Defilement vertical accumule (molette sans Ctrl) : un cran = une image.
        pub wheel: Cell<f64>,
        pub on_resize: RefCell<Option<OnResize>>,
        pub on_view: RefCell<Option<OnView>>,
        pub on_nav: RefCell<Option<OnNav>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "PicmeCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Canvas {}

    impl WidgetImpl for Canvas {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            let s = self.obj().scale_factor().max(1);
            if let Some(cb) = self.on_resize.borrow().as_ref() {
                cb((width * s).max(0) as u32, (height * s).max(0) as u32);
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some((texture, full_w, full_h)) = self.image.borrow().clone() else { return };
            let obj = self.obj();
            let vp = obj.viewport();
            if vp.0 == 0 || vp.1 == 0 {
                return;
            }
            let s = obj.scale_factor().max(1) as f32;
            let view = self.view.get();
            let r = view.rect((full_w, full_h), vp);
            let q = view.quarters();
            // texture dessinee non tournee : pour un quart de tour impair ses cotes sont ceux du rectangle echanges
            let (dw, dh) = if q % 2 == 1 { (r.h as f32, r.w as f32) } else { (r.w as f32, r.h as f32) };
            // reduction (zoom sous le niveau de la texture) : mipmaps ; sinon la copie exacte du fit ou l'agrandissement
            let filter = if texture.width() as f32 > dw + 1.0 { gsk::ScalingFilter::Trilinear } else { gsk::ScalingFilter::Linear };
            snapshot.push_clip(&graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32));
            if q == 0 {
                snapshot.append_scaled_texture(&texture, filter, &graphene::Rect::new(r.x as f32 / s, r.y as f32 / s, dw / s, dh / s));
            } else {
                let center = graphene::Point::new((r.x + r.w / 2.0) as f32 / s, (r.y + r.h / 2.0) as f32 / s);
                snapshot.save();
                snapshot.translate(&center);
                snapshot.rotate(90.0 * q as f32);
                snapshot.append_scaled_texture(&texture, filter, &graphene::Rect::new(-dw / 2.0 / s, -dh / 2.0 / s, dw / s, dh / s));
                snapshot.restore();
            }
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas {
    pub fn new() -> Self {
        let c: Canvas = glib::Object::new();
        c.set_hexpand(true);
        c.set_vexpand(true);
        c.connect_gestures();
        c
    }

    fn connect_gestures(&self) {
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion({
            let c = self.downgrade();
            move |_, x, y| {
                if let Some(c) = c.upgrade() {
                    let s = c.scale_factor().max(1) as f64;
                    c.imp().pointer.set(Some((x * s, y * s)));
                }
            }
        });
        motion.connect_leave({
            let c = self.downgrade();
            move |_| {
                if let Some(c) = c.upgrade() {
                    c.imp().pointer.set(None);
                }
            }
        });
        self.add_controller(motion);

        // molette : Ctrl zoome autour du curseur, seule elle change d'image (un cran, une image)
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let c = self.downgrade();
            move |ctl, _, dy| {
                let Some(c) = c.upgrade() else { return glib::Propagation::Proceed };
                if ctl.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                    c.zoom_by(ZOOM_STEP.powf(-dy));
                } else {
                    let acc = c.imp().wheel.get() + dy;
                    let steps = acc.trunc();
                    c.imp().wheel.set(acc - steps);
                    if steps != 0.0
                        && let Some(cb) = c.imp().on_nav.borrow().as_ref()
                    {
                        cb(steps as isize);
                    }
                }
                glib::Propagation::Stop
            }
        });
        self.add_controller(scroll);

        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        drag.connect_drag_begin({
            let c = self.downgrade();
            move |_, _, _| {
                if let Some(c) = c.upgrade() {
                    c.imp().drag_done.set((0.0, 0.0));
                }
            }
        });
        drag.connect_drag_update({
            let c = self.downgrade();
            move |_, ox, oy| {
                let Some(c) = c.upgrade() else { return };
                let (dx, dy) = c.imp().drag_done.replace((ox, oy));
                let s = c.scale_factor().max(1) as f64;
                let (full, vp) = (c.full(), c.viewport());
                if let Some(full) = full {
                    let mut v = c.imp().view.get();
                    v.pan_by(((ox - dx) * s, (oy - dy) * s), full, vp);
                    c.imp().view.set(v);
                    c.queue_draw();
                }
            }
        });
        self.add_controller(drag);

        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_pressed({
            let c = self.downgrade();
            move |_, n_press, x, y| {
                if n_press == 2
                    && let Some(c) = c.upgrade()
                {
                    let s = c.scale_factor().max(1) as f64;
                    c.toggle_zoom_at(Some((x * s, y * s)));
                }
            }
        });
        self.add_controller(click);
    }

    /// Taille de la vue en device pixels.
    fn viewport(&self) -> (u32, u32) {
        let s = self.scale_factor().max(1);
        ((self.width() * s).max(0) as u32, (self.height() * s).max(0) as u32)
    }

    fn full(&self) -> Option<(u32, u32)> {
        self.imp().image.borrow().as_ref().map(|(_, w, h)| (*w, *h))
    }

    fn anchor(&self) -> (f64, f64) {
        let vp = self.viewport();
        self.imp().pointer.get().unwrap_or((vp.0 as f64 / 2.0, vp.1 as f64 / 2.0))
    }

    /// Apres un changement voulu : redessine et previent (au-dessus du fit ou non).
    fn changed(&self) {
        self.queue_draw();
        let beyond = self.is_zoomed();
        self.imp().beyond.set(beyond);
        if let Some(cb) = self.imp().on_view.borrow().as_ref() {
            cb(beyond);
        }
    }

    pub fn set_image(&self, texture: &gdk::Texture, full_w: u32, full_h: u32) {
        self.imp().image.replace(Some((texture.clone(), full_w, full_h)));
        self.queue_draw();
    }

    pub fn has_image(&self) -> bool {
        self.imp().image.borrow().is_some()
    }

    pub fn clear(&self) {
        if self.imp().image.take().is_some() {
            self.queue_draw();
        }
    }

    pub fn connect_resize(&self, f: impl Fn(u32, u32) + 'static) {
        self.imp().on_resize.replace(Some(Box::new(f)));
    }

    /// Appele apres chaque zoom, rotation ou retour au fit voulus, avec "au-dessus du fit" (le `Full` est alors voulu).
    pub fn connect_view(&self, f: impl Fn(bool) + 'static) {
        self.imp().on_view.replace(Some(Box::new(f)));
    }

    /// Appele par la molette seule : `delta` images (positif = suivante).
    pub fn connect_nav(&self, f: impl Fn(isize) + 'static) {
        self.imp().on_nav.replace(Some(Box::new(f)));
    }

    /// Au-dessus du fit : ce que la texture `Fit` ne suffit plus a servir.
    pub fn is_zoomed(&self) -> bool {
        self.full().is_some_and(|full| !self.imp().view.get().is_fit(full, self.viewport()))
    }

    /// Echelle affichee en pour cent de l'image entiere, si une image est affichee.
    pub fn percent(&self) -> Option<u32> {
        self.full().map(|full| self.imp().view.get().percent(full, self.viewport()))
    }

    pub fn quarters(&self) -> u8 {
        self.imp().view.get().quarters()
    }

    /// Autre image : fit, sans rotation. Sans notification (le controller sait deja que le `Full` est abandonne).
    pub fn reset_view(&self) {
        self.imp().view.set(View::default());
        self.imp().beyond.set(false);
        self.queue_draw();
    }

    /// Previent si un redimensionnement a fait passer la vue de part et d'autre du fit.
    pub fn resync(&self) {
        let beyond = self.is_zoomed();
        if self.imp().beyond.replace(beyond) != beyond {
            self.queue_draw();
            if let Some(cb) = self.imp().on_view.borrow().as_ref() {
                cb(beyond);
            }
        }
    }

    pub fn zoom_by(&self, factor: f64) {
        let Some(full) = self.full() else { return };
        let (anchor, vp) = (self.anchor(), self.viewport());
        let mut v = self.imp().view.get();
        v.zoom_by(factor, anchor, full, vp);
        self.imp().view.set(v);
        self.changed();
    }

    /// Retour au fit sans rotation.
    pub fn fit(&self) {
        if self.full().is_none() {
            return;
        }
        let mut v = self.imp().view.get();
        let q = v.quarters();
        v.reset();
        v.rotate(q as i32);
        self.imp().view.set(v);
        self.changed();
    }

    /// Fit <-> 100 % autour du curseur (au clavier) ou du centre.
    pub fn toggle_zoom(&self) {
        self.toggle_zoom_at(None);
    }

    fn toggle_zoom_at(&self, at: Option<(f64, f64)>) {
        let Some(full) = self.full() else { return };
        let (anchor, vp) = (at.unwrap_or_else(|| self.anchor()), self.viewport());
        let mut v = self.imp().view.get();
        v.toggle(anchor, full, vp);
        self.imp().view.set(v);
        self.changed();
    }

    /// Quarts de tour visuels dans le sens horaire (negatif : anti-horaire).
    pub fn rotate(&self, delta: i32) {
        if self.full().is_none() {
            return;
        }
        let mut v = self.imp().view.get();
        v.rotate(delta);
        self.imp().view.set(v);
        self.changed();
    }
}
