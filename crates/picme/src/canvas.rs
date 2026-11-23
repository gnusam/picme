//! Vue de l'image : une texture dessinee en fit contain, centree, jamais agrandie au-dela de 1:1 en device pixels.
//! La place a l'ecran vient des dimensions de l'image entiere, pas de la texture : un `Preview` de 256 px occupe
//! deja la place exacte du `Fit` qui le remplacera, sans saut. Zoom, pan et rotation viendront au bloc 4.

use gtk4 as gtk;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, prelude::*};

mod imp {
    use super::*;
    use std::cell::RefCell;

    type OnResize = Box<dyn Fn(u32, u32)>;

    #[derive(Default)]
    pub struct Canvas {
        /// Texture et dimensions de l'image entiere.
        pub image: RefCell<Option<(gdk::Texture, u32, u32)>>,
        /// Appele a chaque allocation avec la taille en device pixels.
        pub on_resize: RefCell<Option<OnResize>>,
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
            let s = obj.scale_factor().max(1) as f32;
            let (vw, vh) = ((obj.width() as f32 * s) as u32, (obj.height() as f32 * s) as u32);
            if vw == 0 || vh == 0 {
                return;
            }
            let (dw, dh) = crate::app::fit_dims(full_w, full_h, vw, vh);
            // arrondi au device pixel : un Fit de la taille exacte de l'ecran est copie tel quel
            let x = ((vw - dw) / 2) as f32 / s;
            let y = ((vh - dh) / 2) as f32 / s;
            let rect = graphene::Rect::new(x, y, dw as f32 / s, dh as f32 / s);
            snapshot.append_scaled_texture(&texture, gsk::ScalingFilter::Linear, &rect);
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
        c
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
}
