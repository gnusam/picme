//! Carte du dossier (touche G) : une page HTML autonome, servie en local par `serve` (sur `127.0.0.1`, pour que les
//! tuiles OpenStreetMap recoivent un Referer) et que le navigateur ouvre. Leaflet et les tuiles viennent du navigateur ;
//! picme ne fait aucun appel reseau. Les noms de fichiers arrivent dans la page comme chaines JavaScript echappees et
//! sont poses dans la fenetre d'information par `textContent` : un nom ne peut ni fermer le `<script>` ni injecter de
//! code.

use crate::dir::Entry;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Modele de la page : `@@TITLE@@`, `@@POINTS@@` et `@@CURRENT@@` sont remplaces par `page`. Les empreintes SRI sont
/// celles de leaflet 1.9.4 servi par unpkg.
const TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="fr">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>@@TITLE@@</title>
<link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" integrity="sha256-p4NxAoJBhIIN+hmNHrzRCf9tD/miZyoHS5obTRR9BMY=" crossorigin="">
<style>
html, body, #map { height: 100%; margin: 0; }
.here { width: 22px; height: 22px; box-sizing: border-box; border: 4px solid #fff; border-radius: 50%; background: #d11; box-shadow: 0 0 0 2px #d11, 0 1px 6px rgba(0, 0, 0, 0.6); }
</style>
</head>
<body>
<div id="map"></div>
<script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js" integrity="sha256-20nQCchB9co0qIjJZRGuk2/Z9VM+kNiyxNV1lvTlZBo=" crossorigin=""></script>
<script>
// [nom du fichier, latitude, longitude] ; HERE : indice de l'image affichee, -1 si elle n'a pas de position
const POINTS = @@POINTS@@;
const HERE = @@CURRENT@@;
const map = L.map('map');
L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
  maxZoom: 19,
  referrerPolicy: 'strict-origin-when-cross-origin',
  attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors'
}).addTo(map);
let here = null;
const latlngs = POINTS.map(function (p, i) {
  const ll = [p[1], p[2]];
  const tip = document.createElement('div');
  tip.textContent = p[0];
  const marker = i === HERE
    ? L.marker(ll, { icon: L.divIcon({ className: '', html: '<div class="here"></div>', iconSize: [22, 22] }), zIndexOffset: 1000 })
    : L.marker(ll);
  marker.bindPopup(tip).addTo(map);
  if (i === HERE) here = marker;
  return ll;
});
if (latlngs.length === 1) {
  map.setView(latlngs[0], 16);
} else {
  const bounds = L.latLngBounds(latlngs);
  if (HERE >= 0) {
    // le symetrique de chaque point par rapport a l'image affichee : elle reste au centre, tout le reste dans le cadre
    const c = latlngs[HERE];
    latlngs.forEach(function (p) {
      bounds.extend([Math.max(-85, Math.min(85, 2 * c[0] - p[0])), 2 * c[1] - p[1]]);
    });
  }
  map.fitBounds(bounds, { padding: [40, 40], maxZoom: 16 });
}
if (here) here.openPopup();
</script>
</body>
</html>
"##;

/// Texte HTML : `& < > " '` en entites.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Chaine JavaScript entre guillemets, sure dans un `<script>` : tout ce qui pourrait fermer la balise, l'attribut
/// ou la chaine passe en `\uXXXX`, comme les caracteres de controle et les separateurs de ligne Unicode.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' | '\'' | '<' | '>' | '&' | '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// La page de la carte pour `points` (chemin, latitude, longitude) : un marqueur par photo avec son nom de fichier,
/// l'image `current` (si elle est parmi les points) centree avec un marqueur distinct. Les points hors des bornes
/// des coordonnees sont ecartes : jamais de `NaN` dans la page.
pub fn page(points: &[(PathBuf, f64, f64)], current: Option<&Path>, title: &str) -> String {
    let points: Vec<_> = points.iter().filter(|(_, lat, lon)| lat.abs() <= 90.0 && lon.abs() <= 180.0).collect();
    let rows: Vec<String> = points
        .iter()
        .map(|(path, lat, lon)| {
            let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
            format!("[{}, {lat:.6}, {lon:.6}]", js_string(&name))
        })
        .collect();
    let here = current.and_then(|c| points.iter().position(|(p, _, _)| p == c));
    TEMPLATE
        .replace("@@TITLE@@", &html_escape(title))
        .replace("@@POINTS@@", &format!("[\n{}\n]", rows.join(",\n")))
        .replace("@@CURRENT@@", &here.map_or(-1, |i| i as i64).to_string())
}

/// Lit la position de chaque image de `entries` et rend la page de la carte, que `serve::Server` sert au navigateur
/// (rien n'est ecrit sur le disque). `None` si aucune image n'est geolocalisee. A appeler hors du thread d'interface.
pub fn make(entries: &[Entry], current: Option<&Path>, dir: &Path) -> Option<String> {
    let points = crate::meta::gps_of_entries(entries);
    if points.is_empty() {
        return None;
    }
    let title = format!("picme : {}", dir.file_name().unwrap_or(dir.as_os_str()).to_string_lossy());
    Some(page(&points, current, &title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaKind;
    use crate::meta::tests::{gps_tags, jpeg_with, tiff};
    use crate::testutil::TmpDir;

    fn pt(name: &str, lat: f64, lon: f64) -> (PathBuf, f64, f64) {
        (PathBuf::from("/photos").join(name), lat, lon)
    }

    /// Le contenu du tableau `POINTS` de la page.
    fn points_of(html: &str) -> &str {
        let from = html.find("const POINTS = ").unwrap() + "const POINTS = ".len();
        &html[from..from + html[from..].find(";\n").unwrap()]
    }

    #[test]
    fn a_single_point_is_a_one_row_table_and_current_is_its_index() {
        let p = [pt("a.jpg", 48.858370, 2.294481)];
        let html = page(&p, Some(&p[0].0), "picme : photos");
        assert_eq!(points_of(&html), "[\n[\"a.jpg\", 48.858370, 2.294481]\n]");
        assert!(html.contains("const HERE = 0;"));
        assert!(html.contains("<title>picme : photos</title>"));
        assert!(!html.contains("@@"), "tous les marqueurs de modele sont remplaces");
    }

    #[test]
    fn several_points_keep_their_order_hemispheres_and_six_decimals() {
        let p = [pt("n.jpg", 10.0, 20.0), pt("s.jpg", -33.8688, -151.2093), pt("limit.jpg", 90.0, -180.0), pt("far.jpg", 12.123456789, 0.0)];
        let html = page(&p, Some(&p[1].0), "t");
        assert_eq!(
            points_of(&html),
            "[\n[\"n.jpg\", 10.000000, 20.000000],\n[\"s.jpg\", -33.868800, -151.209300],\n[\"limit.jpg\", 90.000000, -180.000000],\n[\"far.jpg\", 12.123457, 0.000000]\n]"
        );
        assert!(html.contains("const HERE = 1;"));
        assert!(html.contains("fitBounds"));
        // image affichee sans position (ou inconnue) : pas de marqueur distinct
        assert!(page(&p, Some(Path::new("/photos/sans-gps.jpg")), "t").contains("const HERE = -1;"));
        assert!(page(&p, None, "t").contains("const HERE = -1;"));
    }

    #[test]
    fn out_of_range_points_are_dropped() {
        let p = [pt("ok.jpg", 1.0, 2.0), pt("lat.jpg", 91.0, 0.0), pt("lon.jpg", 0.0, -181.0), pt("nan.jpg", f64::NAN, 0.0), pt("inf.jpg", 0.0, f64::INFINITY)];
        let html = page(&p, None, "t");
        assert_eq!(points_of(&html), "[\n[\"ok.jpg\", 1.000000, 2.000000]\n]");
    }

    #[test]
    fn file_names_cannot_inject_script_or_markup() {
        // un nom de fichier ne contient pas de `/` : pas de `</script>` possible, mais tout le reste l'est
        let evil = "\"><img src=x onerror=alert(2)>';\\\n\u{2028}&<script>alert(1)<.jpg";
        let p = [pt(evil, 1.0, 2.0)];
        let html = page(&p, Some(&p[0].0), "</title><script>alert(3)</script>");
        let row = points_of(&html);
        for bad in ["<", ">", "&", "'", "\u{2028}"] {
            assert!(!row.contains(bad), "{bad:?} dans {row}");
        }
        // les deux sauts de ligne du tableau lui-meme, aucun de plus
        assert_eq!(row.matches('\n').count(), 2, "{row}");
        assert!(row.contains("\\u0022") && row.contains("\\u005c") && row.contains("\\u000a") && row.contains("\\u2028"), "{row}");
        // meme la fermeture de balise, qu'un nom ne peut pas porter, est neutralisee
        assert_eq!(js_string("</script>"), "\"\\u003c/script\\u003e\"");
        // le titre est du texte HTML echappe
        assert!(html.contains("<title>&lt;/title&gt;&lt;script&gt;alert(3)&lt;/script&gt;</title>"));
        // le seul <script> ouvrant du gabarit : deux balises, deux fermetures
        assert_eq!(html.matches("<script").count(), 2);
        assert_eq!(html.matches("</script>").count(), 2);
        // le nom entre dans la fenetre d'information comme texte, pas comme HTML
        assert!(html.contains("tip.textContent = p[0];") && !html.contains("innerHTML"));
    }

    #[test]
    fn make_builds_the_page_and_reports_a_folder_without_position() {
        let dir = TmpDir::new("map-make-src");
        let with = |name: &str, lat: (u32, u32, u32, &'static str)| {
            let p = dir.path().join(name);
            std::fs::write(&p, jpeg_with(&tiff(&[], &[], &gps_tags(lat, (2, 0, 0, "E"), None)))).unwrap();
            Entry { path: p, kind: MediaKind::Jpeg, live_mov: None, mtime_ns: 0, size: 0 }
        };
        let plain = {
            let p = dir.path().join("plain.jpg");
            std::fs::copy(crate::fixtures_dir().join("a.jpg"), &p).unwrap();
            Entry { path: p, kind: MediaKind::Jpeg, live_mov: None, mtime_ns: 0, size: 0 }
        };
        // un dossier sans position : pas de page
        assert_eq!(make(std::slice::from_ref(&plain), None, dir.path()), None);
        let (a, b) = (with("a.jpg", (10, 0, 0, "N")), with("b.jpg", (20, 0, 0, "S")));
        let entries = [a.clone(), plain, b];
        let html = make(&entries, Some(&a.path), dir.path()).unwrap();
        assert!(html.contains("\"a.jpg\", 10.000000, 2.000000") && html.contains("\"b.jpg\", -20.000000, 2.000000"), "{html}");
        assert!(!html.contains("plain.jpg") && html.contains("const HERE = 0;"));
        assert!(html.contains(&format!("<title>picme : {}</title>", dir.path().file_name().unwrap().to_string_lossy())));
        // une seconde fois, un autre courant
        assert!(make(&entries, Some(&entries[2].path), dir.path()).unwrap().contains("const HERE = 1;"));
    }
}
