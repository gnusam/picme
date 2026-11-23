//! Affichage discret par-dessus l'image : nom, position et zoom de l'image courante, et les messages et erreurs.
//! Tout s'efface seul apres `OSD_TIMEOUT`.

use crate::chrome::OneShot;
use gtk4 as gtk;
use gtk::{pango, prelude::*};
use std::rc::Rc;
use std::time::Duration;

pub const OSD_TIMEOUT: Duration = Duration::from_secs(2);

/// `nom   n/N   zoom %` ; les parties inconnues (liste pas encore scannee, aucune image) sont omises.
pub fn format_info(name: &str, position: Option<(usize, usize)>, percent: Option<u32>) -> String {
    let mut parts = vec![name.to_string()];
    if let Some((n, total)) = position {
        parts.push(format!("{n}/{total}"));
    }
    if let Some(p) = percent {
        parts.push(format!("{p} %"));
    }
    parts.join("   ")
}

pub struct Osd {
    info: gtk::Label,
    message: gtk::Label,
    info_timer: OneShot,
    message_timer: OneShot,
}

fn label(class: &str, halign: gtk::Align, valign: gtk::Align) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class("osd");
    l.add_css_class(class);
    l.set_halign(halign);
    l.set_valign(valign);
    l.set_margin_start(12);
    l.set_margin_end(12);
    l.set_margin_top(12);
    l.set_margin_bottom(12);
    l.set_ellipsize(pango::EllipsizeMode::Middle);
    l.set_visible(false);
    // jamais sur le chemin d'un clic ou d'une molette
    l.set_can_target(false);
    l.set_focusable(false);
    l
}

impl Osd {
    pub fn new() -> Rc<Osd> {
        let info = label("osd-info", gtk::Align::Start, gtk::Align::Start);
        info.set_max_width_chars(60);
        let message = label("osd-message", gtk::Align::Center, gtk::Align::Start);
        message.set_max_width_chars(90);
        message.set_margin_top(52);
        Rc::new(Osd { info, message, info_timer: OneShot::default(), message_timer: OneShot::default() })
    }

    pub fn attach(&self, overlay: &gtk::Overlay) {
        overlay.add_overlay(&self.info);
        overlay.add_overlay(&self.message);
    }

    pub fn show_info(self: &Rc<Self>, text: &str) {
        Self::show(&self.info, &self.info_timer, text);
    }

    /// Un message ou une erreur, a la place du precedent.
    pub fn message(self: &Rc<Self>, text: &str) {
        Self::show(&self.message, &self.message_timer, text);
    }

    pub fn info_text(&self) -> Option<String> {
        self.info.is_visible().then(|| self.info.text().to_string())
    }

    pub fn message_text(&self) -> Option<String> {
        self.message.is_visible().then(|| self.message.text().to_string())
    }

    fn show(l: &gtk::Label, timer: &OneShot, text: &str) {
        if l.text() != text {
            l.set_text(text);
        }
        l.set_visible(true);
        let l2 = l.clone();
        timer.arm(OSD_TIMEOUT, move || l2.set_visible(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_has_name_position_and_zoom() {
        assert_eq!(format_info("IMG_0001.jpg", Some((3, 120)), Some(25)), "IMG_0001.jpg   3/120   25 %");
    }

    #[test]
    fn unknown_parts_are_left_out() {
        assert_eq!(format_info("a.png", None, None), "a.png");
        assert_eq!(format_info("a.png", Some((1, 1)), None), "a.png   1/1");
        assert_eq!(format_info("a.png", None, Some(100)), "a.png   100 %");
    }
}
