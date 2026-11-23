//! UI qui n'apparait que quand elle sert : le filmstrip se montre 1,5 s apres une navigation ou tant que la souris est
//! pres du bord bas, le curseur disparait apres 2 s sans mouvement.

use gtk4 as gtk;
use gtk::{glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// Le filmstrip reste visible apres une navigation.
pub const STRIP_LINGER: Duration = Duration::from_millis(1500);
/// Curseur masque apres cette inactivite.
pub const CURSOR_IDLE: Duration = Duration::from_secs(2);
/// Hauteur (pixels logiques) du bord bas ou approcher la souris montre le filmstrip.
const NEAR_BOTTOM: f64 = 140.0;

/// Une echeance unique, rearmable : armer remplace la precedente.
#[derive(Default)]
pub struct OneShot(Rc<RefCell<Option<glib::SourceId>>>);

impl OneShot {
    pub fn arm(&self, after: Duration, f: impl FnOnce() + 'static) {
        self.cancel();
        let slot = self.0.clone();
        *self.0.borrow_mut() = Some(glib::timeout_add_local_once(after, move || {
            // la source se termine d'elle-meme : son id est oublie, pas retire
            slot.borrow_mut().take();
            f();
        }));
    }

    pub fn cancel(&self) {
        if let Some(id) = self.0.borrow_mut().take() {
            id.remove();
        }
    }

    pub fn armed(&self) -> bool {
        self.0.borrow().is_some()
    }
}

impl Drop for OneShot {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct Chrome {
    window: gtk::Window,
    strip: gtk::Revealer,
    linger: OneShot,
    idle: OneShot,
    near: Cell<bool>,
    cursor_hidden: Cell<bool>,
    last_pointer: Cell<(f64, f64)>,
}

impl Chrome {
    /// Surveille la souris sur `window` et pilote `strip`.
    pub fn new(window: &gtk::Window, strip: &gtk::Revealer) -> Rc<Chrome> {
        let c = Rc::new(Chrome {
            window: window.clone(),
            strip: strip.clone(),
            linger: OneShot::default(),
            idle: OneShot::default(),
            near: Cell::new(false),
            cursor_hidden: Cell::new(false),
            last_pointer: Cell::new((-1.0, -1.0)),
        });
        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        motion.connect_motion({
            let c = Rc::downgrade(&c);
            move |_, x, y| {
                if let Some(c) = c.upgrade() {
                    c.moved(x, y);
                }
            }
        });
        window.add_controller(motion);
        c.arm_cursor();
        c
    }

    /// Montre le filmstrip `STRIP_LINGER`, apres une navigation.
    pub fn pulse(self: &Rc<Self>) {
        self.strip.set_reveal_child(true);
        if !self.near.get() {
            self.arm_linger();
        }
    }

    fn moved(self: &Rc<Self>, x: f64, y: f64) {
        // un evenement de survol synthetique (la mise en page a change sous le curseur) n'est pas un mouvement
        if self.last_pointer.replace((x, y)) == (x, y) {
            return;
        }
        if self.cursor_hidden.replace(false) {
            self.window.set_cursor(None);
        }
        self.arm_cursor();
        let near = y >= self.window.height() as f64 - NEAR_BOTTOM;
        if near {
            self.linger.cancel();
            self.strip.set_reveal_child(true);
        } else if self.near.get() {
            self.arm_linger();
        }
        self.near.set(near);
    }

    fn arm_linger(self: &Rc<Self>) {
        let c = Rc::downgrade(self);
        self.linger.arm(STRIP_LINGER, move || {
            if let Some(c) = c.upgrade()
                && !c.near.get()
            {
                c.strip.set_reveal_child(false);
            }
        });
    }

    fn arm_cursor(self: &Rc<Self>) {
        let c = Rc::downgrade(self);
        self.idle.arm(CURSOR_IDLE, move || {
            if let Some(c) = c.upgrade() {
                c.cursor_hidden.set(true);
                c.window.set_cursor_from_name(Some("none"));
            }
        });
    }

    pub fn cursor_hidden(&self) -> bool {
        self.cursor_hidden.get()
    }
}
