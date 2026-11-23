//! Lecture seule d'une copie du vrai `~/.cache/thumbnails` (Nautilus et autres) : `target/bench/real-thumbs`, ou le
//! dossier de `PICME_REAL_THUMBS`. Absente, le test s'ignore. La copie ne doit jamais etre ecrite : le test compare
//! l'arborescence avant et apres, et la copie est faite en lecture seule (`chmod -R a-w`).
//!
//! Les fichiers sources n'existent pas ici : chaque entree est reconstruite depuis les chunks de la miniature
//! (`Thumb::URI`, `Thumb::MTime`), ce qui verifie aussi que `thumbs::uri` donne les noms md5 de ces programmes.

use picme_core::dir::Entry;
use picme_core::media::MediaKind;
use picme_core::thumbs::{self, ThumbSize, ThumbStore};
use std::path::{Path, PathBuf};

fn real_root() -> Option<PathBuf> {
    let root = std::env::var_os("PICME_REAL_THUMBS")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bench/real-thumbs"));
    root.is_dir().then_some(root)
}

/// Chemin du PNG, dimensions et chunks tEXt d'une miniature.
type Thumbnail = (PathBuf, (u32, u32), Vec<(String, String)>);

/// Chaque miniature de `<root>/<dir>`.
fn thumbnails(root: &Path, dir: &str) -> Vec<Thumbnail> {
    let Ok(read) = std::fs::read_dir(root.join(dir)) else { return Vec::new() };
    let mut out = Vec::new();
    for e in read.flatten() {
        let path = e.path();
        let Ok(reader) = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap())).read_info() else { continue };
        let text = reader.info().uncompressed_latin1_text.iter().map(|c| (c.keyword.clone(), c.text.clone())).collect();
        out.push((path, (reader.info().width, reader.info().height), text));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn entry_of(text: &[(String, String)]) -> Option<Entry> {
    let get = |k: &str| text.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let uri = get("Thumb::URI")?.strip_prefix("file://")?;
    let path = PathBuf::from(percent_encoding::percent_decode_str(uri).decode_utf8().ok()?.into_owned());
    let secs: i64 = get("Thumb::MTime")?.split('.').next()?.parse().ok()?;
    let kind = picme_core::media::kind_from_path(&path).unwrap_or(MediaKind::Jpeg);
    Some(Entry { path, kind, live_mov: None, mtime_ns: secs * 1_000_000_000, size: get("Thumb::Size").and_then(|s| s.parse().ok()).unwrap_or(0) })
}

/// Chemins relatifs, tailles et dates de tout ce qui est sous `dir`.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    fn rec(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let md = e.metadata().unwrap();
            out.push((e.path().strip_prefix(base).unwrap().to_path_buf(), md.len(), md.modified().unwrap()));
            if md.is_dir() {
                rec(base, &e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    rec(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn real_caches_are_read_validated_and_left_untouched() {
    let Some(root) = real_root() else {
        eprintln!("real_thumbs : pas de copie du cache (target/bench/real-thumbs), test ignore");
        return;
    };
    let before = snapshot(&root);
    let store = ThumbStore::new(&root);
    let mut checked = 0;
    for size in ThumbSize::ALL {
        let dir = match size {
            ThumbSize::Normal => "normal",
            ThumbSize::Large => "large",
            ThumbSize::XLarge => "x-large",
            ThumbSize::XXLarge => "xx-large",
        };
        for (path, (w, h), text) in thumbnails(&root, dir) {
            let Some(entry) = entry_of(&text) else { continue };
            if entry.path.exists() {
                continue; // une vraie source changerait les dimensions entieres : hors du perimetre de ce test
            }
            // le nom md5 que ces programmes ont donne est celui que `thumbs::uri` calcule
            assert_eq!(store.path_of(&entry, size), path, "{} : {}", entry.path.display(), thumbs::uri(&entry.path));

            // lue pour sa propre taille : la miniature elle-meme, a jour, sans signaler de basse resolution
            let got = store.read(&entry, size.px()).unwrap_or_else(|| panic!("{} non lue", path.display()));
            assert_eq!((got.w, got.h), (w, h), "{}", path.display());
            assert!(!got.low_res, "{}", path.display());
            assert_eq!(got.rgba.len(), w as usize * h as usize * 4);

            // perimee a la seconde pres, ou autre fichier de meme nom : ignoree
            let later = Entry { mtime_ns: entry.mtime_ns + 1_000_000_000, ..entry.clone() };
            assert!(store.read(&later, size.px()).is_none(), "{}", path.display());
            assert!(!store.marked_failed(&entry));
            checked += 1;
        }
    }
    eprintln!("real_thumbs : {checked} miniatures verifiees");
    assert_eq!(snapshot(&root), before, "le cache n'a pas a etre ecrit");
}

#[test]
fn real_caches_give_the_biggest_thumbnail_for_a_preview_and_the_smallest_for_a_strip() {
    let Some(root) = real_root() else { return };
    let store = ThumbStore::new(&root);
    let mut both = 0;
    for (_, (nw, nh), text) in thumbnails(&root, "normal") {
        let Some(entry) = entry_of(&text) else { continue };
        if entry.path.exists() {
            continue;
        }
        // une xx-large du meme fichier ? (meme URI, donc meme nom)
        let Some((_, (bw, bh), _)) = thumbnails(&root, "xx-large").into_iter().find(|(p, _, _)| *p == store.path_of(&entry, ThumbSize::XXLarge)) else {
            continue;
        };
        both += 1;
        let preview = store.read(&entry, thumbs::PREVIEW_WANT).unwrap();
        assert_eq!((preview.w, preview.h), (bw, bh));
        // le filmstrip (192 px) a de quoi se contenter de la xx-large quand la large manque, jamais d'agrandissement
        let strip = store.read(&entry, 192).unwrap();
        assert_eq!((strip.w, strip.h), (bw, bh));
        // 128 px : la normal suffit
        let small = store.read(&entry, 128).unwrap();
        assert_eq!((small.w, small.h), (nw, nh));
    }
    eprintln!("real_thumbs : {both} fichiers avec normal et xx-large");
}
