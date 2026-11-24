//! Metadonnees d'une image lues dans son EXIF (kamadak-exif, pur Rust) : les champs du panneau d'informations (I) et
//! la position GPS, seule a servir a la carte du dossier (G). Rien n'est ecrit, et un fichier sans EXIF lisible (PNG
//! sans chunk, video, texte) n'est jamais une erreur : les champs manquent, c'est tout.

use crate::dir::Entry;
use crate::media::MediaKind;
use exif::{Exif, In, Tag, Value};
use std::path::{Path, PathBuf};

/// Etiquette de la ligne de position de `Info::rows` (l'interface y ajoute le rappel de la touche G).
pub const GPS_LABEL: &str = "Position";

/// Position GPS en degres decimaux : latitude positive au nord, longitude positive a l'est.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gps {
    pub lat: f64,
    pub lon: f64,
    /// Metres, negatif sous le niveau de la mer.
    pub alt: Option<f64>,
}

/// Ce que le panneau montre d'un fichier. Les champs EXIF sont deja formates ; `None` quand ils n'existent pas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub name: String,
    /// Octets ; 0 si le fichier n'est plus lisible.
    pub size: u64,
    /// Dimensions affichees (orientation appliquee), lues dans l'en-tete.
    pub dims: Option<(u32, u32)>,
    pub taken: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub focal: Option<String>,
    pub aperture: Option<String>,
    pub exposure: Option<String>,
    pub iso: Option<u32>,
    pub gps: Option<Gps>,
}

impl Info {
    /// Le fichier porte au moins un champ EXIF que le panneau sait montrer.
    pub fn has_exif_fields(&self) -> bool {
        self.taken.is_some()
            || self.camera.is_some()
            || self.lens.is_some()
            || self.focal.is_some()
            || self.aperture.is_some()
            || self.exposure.is_some()
            || self.iso.is_some()
            || self.gps.is_some()
    }

    /// Les lignes (etiquette, valeur) du panneau, dans l'ordre d'affichage, celles qui existent seulement.
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = vec![("Fichier", self.name.clone()), ("Taille", size_text(self.size))];
        if let Some((w, h)) = self.dims {
            rows.push(("Dimensions", format!("{w} x {h}")));
        }
        let text = [
            ("Date de prise de vue", &self.taken),
            ("Appareil", &self.camera),
            ("Objectif", &self.lens),
            ("Focale", &self.focal),
            ("Ouverture", &self.aperture),
            ("Temps de pose", &self.exposure),
        ];
        rows.extend(text.into_iter().filter_map(|(label, v)| Some((label, v.clone()?))));
        if let Some(iso) = self.iso {
            rows.push(("ISO", iso.to_string()));
        }
        if let Some(g) = self.gps {
            rows.push((GPS_LABEL, gps_text(&g)));
        }
        rows
    }
}

/// Taille en Ko, Mo, Go (multiples de 1024, a la francaise), une decimale.
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["Ko", "Mo", "Go"];
    if bytes < 1024 {
        return format!("{bytes} o");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit]).replace('.', ",")
}

/// `48.858370, 2.294481, alt. 35 m` : six decimales (une dizaine de centimetres), signe pour le sud et l'ouest.
fn gps_text(g: &Gps) -> String {
    let mut s = format!("{:.6}, {:.6}", g.lat, g.lon);
    if let Some(alt) = g.alt {
        s.push_str(&format!(", alt. {alt:.0} m"));
    }
    s
}

/// Lit les informations de `path`. Un fichier illisible rend une `Info` avec son seul nom.
pub fn read(path: &Path) -> Info {
    let md = std::fs::metadata(path).ok();
    let mut info = Info {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        size: md.map_or(0, |m| m.len()),
        dims: crate::decode::probe_dims(path),
        ..Info::default()
    };
    if let Some(e) = read_exif(path) {
        info.taken = taken(&e);
        info.camera = camera(&e);
        info.lens = ascii(&e, Tag::LensModel);
        info.focal = focal(&e);
        info.aperture = rational(&e, Tag::FNumber).filter(|&f| f > 0.0).map(|f| format!("f/{}", trim_num(f)));
        info.exposure = rational(&e, Tag::ExposureTime).and_then(exposure_text);
        info.iso = e.get_field(Tag::PhotographicSensitivity, In::PRIMARY).and_then(|f| f.value.get_uint(0)).filter(|&i| i > 0);
        info.gps = gps_of(&e);
    }
    info
}

/// Structure EXIF du conteneur (JPEG, TIFF, PNG, WebP, HEIF selon la crate) ; `None` si absente ou illisible.
fn read_exif(path: &Path) -> Option<Exif> {
    let f = std::fs::File::open(path).ok()?;
    exif::Reader::new().read_from_container(&mut std::io::BufReader::new(f)).ok()
}

/// Texte ASCII d'un tag, sans NUL ni blancs de bourrage ; `None` s'il est vide.
fn ascii(e: &Exif, tag: Tag) -> Option<String> {
    let Value::Ascii(parts) = &e.get_field(tag, In::PRIMARY)?.value else { return None };
    let s = String::from_utf8_lossy(parts.first()?);
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!s.is_empty()).then(|| s.to_string())
}

/// Premier rationnel d'un tag, `None` si le denominateur est nul.
fn rational(e: &Exif, tag: Tag) -> Option<f64> {
    let Value::Rational(v) = &e.get_field(tag, In::PRIMARY)?.value else { return None };
    let r = v.first().filter(|r| r.denom != 0)?;
    Some(r.to_f64())
}

/// `2024:05:01 12:00:00` -> `2024-05-01 12:00:00` ; une date a zero (horloge jamais reglee) n'en est pas une.
fn taken(e: &Exif) -> Option<String> {
    let raw = ascii(e, Tag::DateTimeOriginal).or_else(|| ascii(e, Tag::DateTime))?;
    if raw.starts_with("0000") {
        return None;
    }
    let mut b = raw.into_bytes();
    for i in [4, 7] {
        if b.get(i) == Some(&b':') {
            b[i] = b'-';
        }
    }
    String::from_utf8(b).ok()
}

/// Marque et modele, sans repeter la marque quand le modele la porte deja (`Canon` + `Canon EOS 5D`).
fn camera(e: &Exif) -> Option<String> {
    match (ascii(e, Tag::Make), ascii(e, Tag::Model)) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    }
}

/// `50 mm`, avec l'equivalent 24x36 quand l'appareil le donne : `5.7 mm (equiv. 26 mm)`.
fn focal(e: &Exif) -> Option<String> {
    let f = rational(e, Tag::FocalLength).filter(|&f| f > 0.0)?;
    let mut s = format!("{} mm", trim_num(f));
    let eq = e.get_field(Tag::FocalLengthIn35mmFilm, In::PRIMARY).and_then(|f| f.value.get_uint(0)).filter(|&v| v > 0);
    if let Some(eq) = eq.filter(|&v| f64::from(v) != f) {
        s.push_str(&format!(" (équiv. {eq} mm)"));
    }
    Some(s)
}

/// Une decimale, sans `.0` : `2.8`, `50`.
fn trim_num(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// `1/250 s` sous la seconde, `2 s` ou `2.5 s` au-dela.
fn exposure_text(t: f64) -> Option<String> {
    if !t.is_finite() || t <= 0.0 {
        return None;
    }
    Some(if t < 0.95 { format!("1/{} s", (1.0 / t).round()) } else { format!("{} s", trim_num(t)) })
}

/// Degres decimaux d'une coordonnee : `tag` donne degres, minutes, secondes (un a trois rationnels), `ref_tag` la
/// lettre de l'hemisphere (`pos` ou `neg`). `None` si une piece manque, si un denominateur est nul ou si la lettre
/// est autre.
fn coord(e: &Exif, tag: Tag, ref_tag: Tag, pos: u8, neg: u8, limit: f64) -> Option<f64> {
    let Value::Rational(parts) = &e.get_field(tag, In::PRIMARY)?.value else { return None };
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|r| r.denom == 0) {
        return None;
    }
    let abs: f64 = parts.iter().zip([1.0, 60.0, 3600.0]).map(|(r, div)| r.to_f64() / div).sum();
    let Value::Ascii(letters) = &e.get_field(ref_tag, In::PRIMARY)?.value else { return None };
    let sign = match letters.first()?.first()?.to_ascii_uppercase() {
        l if l == pos => 1.0,
        l if l == neg => -1.0,
        _ => return None,
    };
    (abs.is_finite() && abs <= limit).then_some(sign * abs)
}

/// Position GPS de la structure EXIF ; l'altitude est facultative (`GPSAltitudeRef` 1 : sous le niveau de la mer).
fn gps_of(e: &Exif) -> Option<Gps> {
    let lat = coord(e, Tag::GPSLatitude, Tag::GPSLatitudeRef, b'N', b'S', 90.0)?;
    let lon = coord(e, Tag::GPSLongitude, Tag::GPSLongitudeRef, b'E', b'W', 180.0)?;
    let below = e.get_field(Tag::GPSAltitudeRef, In::PRIMARY).and_then(|f| f.value.get_uint(0)) == Some(1);
    let alt = rational(e, Tag::GPSAltitude).filter(|a| a.is_finite()).map(|a| if below { -a } else { a });
    Some(Gps { lat, lon, alt })
}

/// Position GPS de chaque image geolocalisee de `entries`, dans l'ordre de la liste : (chemin, latitude, longitude).
/// Seul l'EXIF est lu (l'en-tete d'un JPEG), les videos et les fichiers sans position ou illisibles sont ignores
/// sans erreur. A appeler hors du thread d'interface : un dossier compte des milliers de fichiers.
pub fn gps_of_entries(entries: &[Entry]) -> Vec<(PathBuf, f64, f64)> {
    entries
        .iter()
        .filter(|e| e.kind != MediaKind::Video)
        .filter_map(|e| {
            let g = gps_of(&read_exif(&e.path)?)?;
            Some((e.path.clone(), g.lat, g.lon))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    /// Valeur d'un tag de la structure TIFF fabriquee par `tiff`.
    #[derive(Clone)]
    pub enum V {
        Ascii(&'static str),
        Byte(u8),
        Short(u16),
        Long(u32),
        Rat(Vec<(u32, u32)>),
    }

    fn put_ifd(out: &mut Vec<u8>, entries: &[(u16, V)], data: &mut Vec<u8>, data_base: usize) {
        out.extend((entries.len() as u16).to_be_bytes());
        for (tag, v) in entries {
            let (typ, count, bytes): (u16, u32, Vec<u8>) = match v {
                V::Ascii(s) => (2, s.len() as u32 + 1, [s.as_bytes(), &[0]].concat()),
                V::Byte(b) => (1, 1, vec![*b]),
                V::Short(s) => (3, 1, s.to_be_bytes().to_vec()),
                V::Long(l) => (4, 1, l.to_be_bytes().to_vec()),
                V::Rat(r) => (5, r.len() as u32, r.iter().flat_map(|(n, d)| [n.to_be_bytes(), d.to_be_bytes()].concat()).collect()),
            };
            out.extend(tag.to_be_bytes());
            out.extend(typ.to_be_bytes());
            out.extend(count.to_be_bytes());
            if bytes.len() <= 4 {
                // valeur dans l'entree, cadree a gauche
                out.extend(&bytes);
                out.extend(vec![0; 4 - bytes.len()]);
            } else {
                out.extend(((data_base + data.len()) as u32).to_be_bytes());
                data.extend(&bytes);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
        }
        out.extend([0; 4]);
    }

    /// Structure TIFF big-endian : IFD0, puis IFD EXIF et IFD GPS s'ils ne sont pas vides (les pointeurs sont ajoutes).
    pub fn tiff(ifd0: &[(u16, V)], exif: &[(u16, V)], gps: &[(u16, V)]) -> Vec<u8> {
        let size = |n: usize| 2 + 12 * n + 4;
        let (has_exif, has_gps) = (!exif.is_empty(), !gps.is_empty());
        let exif_at = 8 + size(ifd0.len() + usize::from(has_exif) + usize::from(has_gps));
        let gps_at = exif_at + if has_exif { size(exif.len()) } else { 0 };
        let data_base = gps_at + if has_gps { size(gps.len()) } else { 0 };
        let mut ifd0 = ifd0.to_vec();
        if has_exif {
            ifd0.push((0x8769, V::Long(exif_at as u32)));
        }
        if has_gps {
            ifd0.push((0x8825, V::Long(gps_at as u32)));
        }
        ifd0.sort_by_key(|(t, _)| *t);
        let mut out = b"MM\0*\0\0\0\x08".to_vec();
        let mut data = Vec::new();
        put_ifd(&mut out, &ifd0, &mut data, data_base);
        if has_exif {
            put_ifd(&mut out, exif, &mut data, data_base);
        }
        if has_gps {
            put_ifd(&mut out, gps, &mut data, data_base);
        }
        out.extend(data);
        out
    }

    /// `a.jpg` avec un APP1 EXIF de plus juste apres le SOI.
    pub fn jpeg_with(tiff: &[u8]) -> Vec<u8> {
        let src = std::fs::read(crate::fixtures_dir().join("a.jpg")).unwrap();
        let len = u16::try_from(2 + 6 + tiff.len()).unwrap();
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend(len.to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(tiff);
        out.extend(&src[2..]);
        out
    }

    fn dms(d: u32, m: u32, s: u32) -> V {
        V::Rat(vec![(d, 1), (m, 1), (s, 1)])
    }

    /// Tags GPS d'une position `lat` `lon` (degres, minutes, secondes) dans les hemispheres donnes.
    pub fn gps_tags(lat: (u32, u32, u32, &'static str), lon: (u32, u32, u32, &'static str), alt: Option<(u32, u8)>) -> Vec<(u16, V)> {
        let mut t = vec![
            (1, V::Ascii(lat.3)),
            (2, dms(lat.0, lat.1, lat.2)),
            (3, V::Ascii(lon.3)),
            (4, dms(lon.0, lon.1, lon.2)),
        ];
        if let Some((a, below)) = alt {
            t.push((5, V::Byte(below)));
            t.push((6, V::Rat(vec![(a * 10, 10)])));
        }
        t
    }

    fn write(dir: &TmpDir, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn full_tiff() -> Vec<u8> {
        tiff(
            &[(0x010F, V::Ascii("Canon")), (0x0110, V::Ascii("Canon EOS 5D"))],
            &[
                (0x9003, V::Ascii("2024:05:01 12:34:56")),
                (0x829A, V::Rat(vec![(1, 250)])),
                (0x829D, V::Rat(vec![(28, 10)])),
                (0x8827, V::Short(400)),
                (0x920A, V::Rat(vec![(50, 1)])),
                (0xA434, V::Ascii("EF50mm f/1.8")),
            ],
            // 48 51 30 N, 2 17 40 E, 35 m
            &gps_tags((48, 51, 30, "N"), (2, 17, 40, "E"), Some((35, 0))),
        )
    }

    #[test]
    fn every_field_is_read_and_formatted() {
        let dir = TmpDir::new("meta-full");
        let p = write(&dir, "p.jpg", &jpeg_with(&full_tiff()));
        let info = read(&p);
        assert_eq!(info.name, "p.jpg");
        assert!(info.size > 0 && info.dims.is_some());
        assert_eq!(info.taken.as_deref(), Some("2024-05-01 12:34:56"));
        assert_eq!(info.camera.as_deref(), Some("Canon EOS 5D"), "la marque n'est pas repetee");
        assert_eq!(info.lens.as_deref(), Some("EF50mm f/1.8"));
        assert_eq!(info.focal.as_deref(), Some("50 mm"));
        assert_eq!(info.aperture.as_deref(), Some("f/2.8"));
        assert_eq!(info.exposure.as_deref(), Some("1/250 s"));
        assert_eq!(info.iso, Some(400));
        let g = info.gps.unwrap();
        assert!((g.lat - (48.0 + 51.0 / 60.0 + 30.0 / 3600.0)).abs() < 1e-9, "{g:?}");
        assert!((g.lon - (2.0 + 17.0 / 60.0 + 40.0 / 3600.0)).abs() < 1e-9, "{g:?}");
        assert_eq!(g.alt, Some(35.0));
        let rows = info.rows();
        let labels: Vec<_> = rows.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            ["Fichier", "Taille", "Dimensions", "Date de prise de vue", "Appareil", "Objectif", "Focale", "Ouverture", "Temps de pose", "ISO", GPS_LABEL]
        );
        assert_eq!(rows.last().unwrap().1, "48.858333, 2.294444, alt. 35 m");
    }

    #[test]
    fn southern_and_western_hemispheres_are_negative_and_altitude_can_be_below_sea_level() {
        let dir = TmpDir::new("meta-hemi");
        let t = tiff(&[], &[], &gps_tags((33, 52, 0, "S"), (151, 12, 0, "W"), Some((12, 1))));
        let g = read(&write(&dir, "sw.jpg", &jpeg_with(&t))).gps.unwrap();
        assert!(g.lat < 0.0 && g.lon < 0.0, "{g:?}");
        assert!((g.lat + 33.866667).abs() < 1e-6 && (g.lon + 151.2).abs() < 1e-9, "{g:?}");
        assert_eq!(g.alt, Some(-12.0));
        // sans altitude : la ligne n'en dit rien
        let t = tiff(&[], &[], &gps_tags((0, 0, 0, "N"), (180, 0, 0, "E"), None));
        let g = read(&write(&dir, "limit.jpg", &jpeg_with(&t))).gps.unwrap();
        assert_eq!((g.lat, g.lon, g.alt), (0.0, 180.0, None));
        assert_eq!(gps_text(&g), "0.000000, 180.000000");
    }

    #[test]
    fn invalid_coordinates_are_not_a_position() {
        let dir = TmpDir::new("meta-invalid");
        for (name, tags) in [
            ("lat91", gps_tags((91, 0, 0, "N"), (2, 0, 0, "E"), None)),
            ("lon181", gps_tags((45, 0, 0, "N"), (181, 0, 0, "E"), None)),
            ("ref_x", gps_tags((45, 0, 0, "X"), (2, 0, 0, "E"), None)),
        ] {
            let t = tiff(&[], &[], &tags);
            assert_eq!(read(&write(&dir, &format!("{name}.jpg"), &jpeg_with(&t))).gps, None, "{name}");
        }
        // latitude sans longitude
        let t = tiff(&[], &[], &gps_tags((45, 0, 0, "N"), (2, 0, 0, "E"), None)[..2]);
        assert_eq!(read(&write(&dir, "half.jpg", &jpeg_with(&t))).gps, None);
        // denominateur nul
        let t = tiff(&[], &[], &[(1, V::Ascii("N")), (2, V::Rat(vec![(1, 0)])), (3, V::Ascii("E")), (4, V::Rat(vec![(1, 1)]))]);
        assert_eq!(read(&write(&dir, "zero.jpg", &jpeg_with(&t))).gps, None);
    }

    #[test]
    fn exposure_and_focal_formats() {
        assert_eq!(exposure_text(1.0 / 4000.0).as_deref(), Some("1/4000 s"));
        assert_eq!(exposure_text(0.3).as_deref(), Some("1/3 s"));
        assert_eq!(exposure_text(2.0).as_deref(), Some("2 s"));
        assert_eq!(exposure_text(2.5).as_deref(), Some("2.5 s"));
        assert_eq!(exposure_text(0.0), None);
        assert_eq!(size_text(512), "512 o");
        assert_eq!(size_text(2_500_000), "2,4 Mo");
        let dir = TmpDir::new("meta-focal");
        let t = tiff(&[], &[(0x920A, V::Rat(vec![(57, 10)])), (0xA405, V::Short(26))], &[]);
        let info = read(&write(&dir, "f.jpg", &jpeg_with(&t)));
        assert_eq!(info.focal.as_deref(), Some("5.7 mm (équiv. 26 mm)"));
    }

    #[test]
    fn files_without_readable_exif_give_only_the_file_rows() {
        let dir = TmpDir::new("meta-none");
        for name in ["a.jpg", "d.png", "j.mov", "e.tiff", "f.webp"] {
            let info = read(&crate::fixtures_dir().join(name));
            assert_eq!(info.name, name);
            assert!(info.gps.is_none(), "{name}");
            assert!(info.rows().len() >= 2);
        }
        assert!(!read(&crate::fixtures_dir().join("a.jpg")).has_exif_fields());
        let text = write(&dir, "notes.txt", b"pas une image");
        let info = read(&text);
        assert!(!info.has_exif_fields());
        assert_eq!(info.rows(), [("Fichier", "notes.txt".to_string()), ("Taille", "13 o".to_string())]);
        // fichier disparu : le nom reste, rien ne panique
        let gone = read(&dir.path().join("absent.jpg"));
        assert_eq!((gone.name.as_str(), gone.size, gone.has_exif_fields()), ("absent.jpg", 0, false));
    }

    fn entry(path: PathBuf, kind: MediaKind) -> Entry {
        Entry { path, kind, live_mov: None, mtime_ns: 0, size: 0 }
    }

    #[test]
    fn gps_of_entries_keeps_geolocated_images_in_list_order() {
        let dir = TmpDir::new("meta-list");
        let north = write(&dir, "1_north.jpg", &jpeg_with(&tiff(&[], &[], &gps_tags((10, 0, 0, "N"), (20, 0, 0, "E"), None))));
        let south = write(&dir, "3_south.jpg", &jpeg_with(&tiff(&[], &[], &gps_tags((10, 30, 0, "S"), (20, 0, 0, "W"), None))));
        let plain = write(&dir, "2_plain.jpg", &std::fs::read(crate::fixtures_dir().join("a.jpg")).unwrap());
        let png = write(&dir, "4.png", &std::fs::read(crate::fixtures_dir().join("d.png")).unwrap());
        let junk = write(&dir, "5_junk.jpg", b"ceci n'est pas un jpeg");
        // une video porteuse d'une structure EXIF valide : ignoree par son genre
        let video = write(&dir, "6.mov", &jpeg_with(&tiff(&[], &[], &gps_tags((1, 0, 0, "N"), (1, 0, 0, "E"), None))));
        let entries = [
            entry(north.clone(), MediaKind::Jpeg),
            entry(plain, MediaKind::Jpeg),
            entry(south.clone(), MediaKind::Jpeg),
            entry(png, MediaKind::Png),
            entry(junk, MediaKind::Jpeg),
            entry(video, MediaKind::Video),
            entry(dir.path().join("absent.jpg"), MediaKind::Jpeg),
        ];
        assert_eq!(gps_of_entries(&entries), [(north, 10.0, 20.0), (south, -10.5, -20.0)]);
        assert!(gps_of_entries(&[]).is_empty());
    }
}
