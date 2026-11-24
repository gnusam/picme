//! Panneau d'informations de l'image courante : un panneau centre par-dessus l'image, comme l'aide, ouvert par I et
//! ferme par n'importe quelle touche. La lecture des metadonnees (`picme_core::meta`) se fait hors du thread
//! d'interface, a la demande ; ce module ne fait que montrer le resultat.

use gtk4 as gtk;
use gtk::prelude::*;
use picme_core::meta::{GPS_LABEL, Info};

pub struct InfoPanel {
    label: gtk::Label,
}

impl InfoPanel {
    pub fn new() -> InfoPanel {
        let label = gtk::Label::new(None);
        label.add_css_class("osd");
        label.add_css_class("help");
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_use_markup(true);
        label.set_justify(gtk::Justification::Left);
        // un nom de fichier long passe a la ligne au lieu d'elargir le panneau au-dela de la fenetre
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::Char);
        label.set_max_width_chars(80);
        label.set_visible(false);
        label.set_can_target(false);
        label.set_focusable(false);
        InfoPanel { label }
    }

    pub fn attach(&self, overlay: &gtk::Overlay) {
        overlay.add_overlay(&self.label);
    }

    pub fn visible(&self) -> bool {
        self.label.is_visible()
    }

    pub fn show(&self, info: &Info) {
        self.label.set_markup(&markup(info));
        self.label.set_visible(true);
    }

    pub fn hide(&self) {
        self.label.set_visible(false);
    }
}

impl Default for InfoPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// Deux colonnes en monospace, la largeur des etiquettes calculee sur la plus longue. Sans champ EXIF, une ligne le
/// dit ; la ligne de position rappelle la touche de la carte.
fn markup(info: &Info) -> String {
    let rows = info.rows();
    let width = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    let mut out = String::from("<span font_family=\"monospace\"><b>Informations</b>\n\n");
    for (label, value) in &rows {
        let pad = " ".repeat(width - label.chars().count());
        out.push_str(&format!("<b>{}</b>{pad}   {}", esc(label), esc(value)));
        if *label == GPS_LABEL {
            out.push_str("  <i>(G : carte du dossier)</i>");
        }
        out.push('\n');
    }
    if !info.has_exif_fields() {
        out.push_str("\n<i>pas de métadonnées EXIF lisibles</i>\n");
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
    use picme_core::meta::Gps;

    fn info() -> Info {
        Info { name: "a&b<1>.jpg".into(), size: 2048, dims: Some((4000, 3000)), ..Info::default() }
    }

    #[test]
    fn an_image_without_exif_says_so_in_one_line() {
        let m = markup(&info());
        assert!(m.contains("pas de métadonnées EXIF lisibles"));
        assert!(m.contains("a&amp;b&lt;1&gt;.jpg"), "{m}");
        assert!(m.contains("4000 x 3000") && m.contains("2,0 Ko"));
        assert!(!m.contains("carte du dossier"));
    }

    #[test]
    fn the_position_row_reminds_the_map_key_and_the_missing_note_goes() {
        let m = markup(&Info { iso: Some(200), gps: Some(Gps { lat: -1.5, lon: 2.0, alt: None }), ..info() });
        assert!(m.contains("-1.500000, 2.000000  <i>(G : carte du dossier)</i>"), "{m}");
        assert!(m.contains("ISO") && !m.contains("pas de métadonnées"));
    }
}
