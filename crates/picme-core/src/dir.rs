//! Listing d'un dossier : filtre par extension, tri naturel insensible a la casse, pairing Live Photo.

use crate::media::{MediaKind, kind_from_path};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Une entree de la liste : une image ou une video, avec son `.mov` de Live Photo le cas echeant.
/// `mtime_ns` et `size` sont ceux du fichier principal ; ils entrent dans la cle de cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub live_mov: Option<PathBuf>,
    pub mtime_ns: i64,
    pub size: u64,
}

/// Entree d'un seul fichier (re-stat apres un evenement du dossier). `None` si le fichier a disparu, n'est pas
/// un fichier regulier ou n'a pas une extension de media. Le `kind` vient de l'extension : le listing n'ouvre
/// aucun fichier, `decode` re-detecte par magic bytes.
pub fn stat_entry(path: &Path) -> Option<Entry> {
    let kind = kind_from_path(path)?;
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    Some(Entry {
        path: path.to_path_buf(),
        kind,
        live_mov: None,
        mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        size: md.len(),
    })
}

/// Liste les images et videos d'un dossier (sans recursion, fichiers caches ignores), triees par nom en ordre
/// naturel. Une image et un `.mov` de meme stem ne font qu'une entree (Live Photo) ; un `.mov` seul reste une video.
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let path = item?.path();
        if path.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b".")) {
            continue;
        }
        if let Some(e) = stat_entry(&path) {
            entries.push(e);
        }
    }
    entries.sort_by(|a, b| natural_cmp(&file_name(&a.path), &file_name(&b.path)));
    Ok(pair_live_photos(entries))
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn stem_key(p: &Path) -> String {
    p.file_stem().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn is_mov(e: &Entry) -> bool {
    e.path.extension().is_some_and(|x| x.eq_ignore_ascii_case("mov"))
}

/// `entries` est deja trie. Un `.mov` est rattache a la premiere image (hors video) de meme stem, casse ignoree
/// (IMG_0001.HEIC et IMG_0001.MOV chez Apple) ; l'ordre de la liste reste celui des images.
fn pair_live_photos(entries: Vec<Entry>) -> Vec<Entry> {
    let mut image_of_stem: HashMap<String, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.kind != MediaKind::Video {
            image_of_stem.entry(stem_key(&e.path)).or_insert(i);
        }
    }
    let mut live: HashMap<usize, PathBuf> = HashMap::new();
    let mut paired = vec![false; entries.len()];
    for (i, e) in entries.iter().enumerate() {
        if is_mov(e)
            && e.kind == MediaKind::Video
            && let Some(&img) = image_of_stem.get(&stem_key(&e.path))
        {
            live.entry(img).or_insert_with(|| e.path.clone());
            paired[i] = live[&img] == e.path;
        }
    }
    entries
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !paired[*i])
        .map(|(i, mut e)| {
            e.live_mov = live.remove(&i);
            e
        })
        .collect()
}

/// Tri naturel insensible a la casse : les suites de chiffres se comparent par valeur (`img2` < `img10`).
/// Ordre total : a egalite, l'ordre des octets departage (`a01` et `a1`, `A` et `a`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut sa, mut sb) = (a, b);
    loop {
        match (sa.chars().next(), sb.chars().next()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let (da, ra) = split_digits(sa);
                let (db, rb) = split_digits(sb);
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
                sa = ra;
                sb = rb;
            }
            (Some(ca), Some(cb)) => {
                let ord = ca.to_lowercase().cmp(cb.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                sa = &sa[ca.len_utf8()..];
                sb = &sb[cb.len_utf8()..];
            }
        }
    }
    a.cmp(b)
}

fn split_digits(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    #[test]
    fn natural_order_compares_digit_runs_by_value() {
        let mut v = vec!["img10.jpg", "img2.jpg", "IMG1.jpg", "img_3.jpg", "Img02.jpg", "img0010.jpg"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["IMG1.jpg", "Img02.jpg", "img2.jpg", "img0010.jpg", "img10.jpg", "img_3.jpg"]);
    }

    #[test]
    fn natural_order_is_case_insensitive_and_total() {
        assert_eq!(natural_cmp("a.jpg", "B.jpg"), Ordering::Less);
        assert_eq!(natural_cmp("B.jpg", "a.jpg"), Ordering::Greater);
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
        // memes cles naturelles, departages par les octets : jamais Equal pour deux noms distincts
        assert_ne!(natural_cmp("a1", "a01"), Ordering::Equal);
        assert_ne!(natural_cmp("A1", "a1"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "a1"), Ordering::Less);
        assert_eq!(natural_cmp("é1", "é2"), Ordering::Less);
    }

    #[test]
    fn lists_filters_sorts_and_pairs_live_photos() {
        let dir = TmpDir::new("dir-list");
        for name in [
            "IMG_10.jpg", "IMG_2.JPG", "IMG_1.HEIC", "IMG_1.MOV", "solo.mov", "pair.png", "pair.mov", "notes.txt",
            ".hidden.jpg", "clip.mp4",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub.jpg")).unwrap();

        let list = list(dir.path()).unwrap();
        let names: Vec<String> = list.iter().map(|e| file_name(&e.path)).collect();
        assert_eq!(names, ["clip.mp4", "IMG_1.HEIC", "IMG_2.JPG", "IMG_10.jpg", "pair.png", "solo.mov"]);

        let by_name = |n: &str| list.iter().find(|e| file_name(&e.path) == n).unwrap();
        assert_eq!(by_name("IMG_1.HEIC").live_mov.as_deref(), Some(dir.path().join("IMG_1.MOV").as_path()));
        assert_eq!(by_name("IMG_1.HEIC").kind, MediaKind::Heif);
        assert_eq!(by_name("pair.png").live_mov.as_deref(), Some(dir.path().join("pair.mov").as_path()));
        assert_eq!(by_name("solo.mov").kind, MediaKind::Video);
        assert_eq!(by_name("solo.mov").live_mov, None);
        assert_eq!(by_name("IMG_2.JPG").size, 1);
        assert!(by_name("IMG_2.JPG").mtime_ns > 0);
    }

    #[test]
    fn stat_entry_rejects_missing_and_unknown() {
        let dir = TmpDir::new("dir-stat");
        assert_eq!(stat_entry(&dir.path().join("absent.jpg")), None);
        let txt = dir.path().join("a.txt");
        std::fs::write(&txt, b"x").unwrap();
        assert_eq!(stat_entry(&txt), None);
        let jpg = dir.path().join("a.jpg");
        std::fs::write(&jpg, b"xyz").unwrap();
        let e = stat_entry(&jpg).unwrap();
        assert_eq!((e.kind, e.size, e.live_mov), (MediaKind::Jpeg, 3, None));
    }
}
