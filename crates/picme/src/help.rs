//! Aide des raccourcis : un panneau centre par-dessus l'image, ouvert par ? (ou F1), ferme par n'importe quelle touche.

use gtk4 as gtk;
use gtk::prelude::*;

/// (touches, action) ; une ligne vide separe les groupes. A garder alignee sur `keys::action` et le README.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Droite  Espace  Page suiv.", "image suivante"),
    ("Gauche  Retour  Page préc.", "image précédente"),
    ("Home  End", "première, dernière image"),
    ("", ""),
    ("F", "ajusté / 100 %"),
    ("+  -  0", "zoom avant, arrière, ajusté"),
    ("R  Maj+R", "tourne le fichier à droite, à gauche"),
    ("Ctrl+Droite  Ctrl+Gauche", "tourne la vue seulement"),
    ("C", "recadrage (Entrée copie, Maj+Entrée remplace)"),
    ("", ""),
    ("Suppr", "à la corbeille"),
    ("Maj+Suppr", "supprime (définitif après 30 s)"),
    ("Ctrl+Z", "annule la suppression (30 s)"),
    ("O", "ouvre le dossier dans l'explorateur"),
    ("I", "informations EXIF de l'image"),
    ("G", "carte des photos géolocalisées du dossier"),
    ("", ""),
    ("Espace", "vidéo : pause, reprise"),
    ("Maj+Droite  Maj+Gauche", "vidéo : saute de 5 s"),
    ("M", "vidéo : son"),
    ("L (tenue)", "joue la vidéo d'une Live Photo"),
    ("", ""),
    ("F11", "plein écran"),
    ("?  F1", "cette aide"),
    ("Q  Échap", "quitter"),
];

pub struct Help {
    label: gtk::Label,
}

impl Help {
    pub fn new() -> Help {
        let label = gtk::Label::new(None);
        label.add_css_class("osd");
        label.add_css_class("help");
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_use_markup(true);
        label.set_justify(gtk::Justification::Left);
        label.set_markup(&markup());
        label.set_visible(false);
        label.set_can_target(false);
        label.set_focusable(false);
        Help { label }
    }

    pub fn attach(&self, overlay: &gtk::Overlay) {
        overlay.add_overlay(&self.label);
    }

    pub fn visible(&self) -> bool {
        self.label.is_visible()
    }

    pub fn toggle(&self) {
        self.label.set_visible(!self.label.is_visible());
    }

    pub fn hide(&self) {
        self.label.set_visible(false);
    }
}

impl Default for Help {
    fn default() -> Self {
        Self::new()
    }
}

/// Deux colonnes en monospace, la largeur des touches calculee sur la plus longue.
fn markup() -> String {
    let width = SHORTCUTS.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let mut out = String::from("<span font_family=\"monospace\"><b>Raccourcis</b>\n\n");
    for (keys, what) in SHORTCUTS {
        if keys.is_empty() {
            out.push('\n');
            continue;
        }
        let pad = " ".repeat(width - keys.chars().count());
        out.push_str(&format!("<b>{}</b>{pad}   {}\n", esc(keys), esc(what)));
    }
    out.push_str("\n<i>une touche pour fermer</i></span>");
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_has_keys_and_a_description() {
        for (k, w) in SHORTCUTS {
            assert_eq!(k.is_empty(), w.is_empty(), "{k:?} / {w:?}");
        }
        assert!(markup().contains("cette aide"));
    }
}
