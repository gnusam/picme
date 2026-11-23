//! Listing d'un dossier : filtre par extension, tri naturel insensible a la casse, pairing Live Photo.

use crate::media::{MediaKind, kind_from_path};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Taille au-dessous de laquelle un `.mov` de meme nom qu'une image est le clip de sa Live Photo (3 Mo environ chez
/// Apple) ; au-dela, c'est une video autonome qui garde son entree.
pub const LIVE_MOV_MAX: u64 = 15 << 20;

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
/// naturel. Une image et un `.mov` de meme stem ne font qu'une entree (Live Photo) si l'image est la seule de ce stem
/// et le `.mov` petit (`LIVE_MOV_MAX`) ; sinon le `.mov` reste une video.
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

/// Insere `entry` a sa place dans une liste triee par `list` ; rend son index.
pub fn insert_sorted(entries: &mut Vec<Entry>, entry: Entry) -> usize {
    let name = file_name(&entry.path);
    let i = entries.partition_point(|e| natural_cmp(&file_name(&e.path), &name) == Ordering::Less);
    entries.insert(i, entry);
    i
}

/// Ce que `reconcile` a change dans la liste.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Entrees retirees, avec l'index qu'elles occupaient au moment du retrait.
    pub removed: Vec<(usize, Entry)>,
    /// Anciennes versions des entrees dont le fichier a change (mtime ou taille) : leurs cles de cache sont perimees.
    pub updated: Vec<Entry>,
    /// La liste a change d'une facon ou d'une autre (insertion, retrait, modification, `.mov` de Live Photo).
    pub changed: bool,
}

/// Remet la liste (issue de `list` sur le dossier de `path`) en accord avec l'etat disque de `path`, par un seul stat.
/// Idempotent : un second appel sans changement sur le disque ne fait rien. Memes regles que `list` : fichiers
/// caches ignores, un `.mov` rejoint l'image de meme stem (Live Photo, aux conditions de `list`), et redevient une
/// video seule si l'image part, si une seconde image du meme stem arrive ou si le `.mov` grossit.
pub fn reconcile(entries: &mut Vec<Entry>, path: &Path) -> Reconciled {
    let mut out = Reconciled::default();
    let now = stat_entry(path);
    if path.file_name().is_none_or(|n| n.as_encoded_bytes().starts_with(b".")) {
        return out;
    }
    let stem = stem_key(path);
    reconcile_path(entries, path, now, &mut out);
    restitch(entries, &stem, &mut out);
    out
}

/// L'etat de `path` seul : insertion, mise a jour ou retrait de son entree, sans regarder les appariements du stem.
fn reconcile_path(entries: &mut Vec<Entry>, path: &Path, now: Option<Entry>, out: &mut Reconciled) {
    if let Some(i) = entries.iter().position(|e| e.path == path) {
        match now {
            None => {
                let old = entries.remove(i);
                // le .mov d'une Live Photo dont l'image a disparu redevient une entree a part entiere
                if let Some(mov) = old.live_mov.as_deref().and_then(stat_entry) {
                    insert_sorted(entries, mov);
                }
                out.removed.push((i, old));
                out.changed = true;
            }
            Some(new) if (new.mtime_ns, new.size) != (entries[i].mtime_ns, entries[i].size) => {
                let live_mov = entries[i].live_mov.take();
                let old = std::mem::replace(&mut entries[i], Entry { live_mov, ..new });
                out.updated.push(Entry { live_mov: None, ..old });
                out.changed = true;
            }
            Some(_) => {}
        }
        return;
    }
    if let Some(i) = entries.iter().position(|e| e.live_mov.as_deref() == Some(path)) {
        // le .mov d'une Live Photo : sa disparition change la liste, et sa croissance au-dela d'un clip de Live Photo
        // en refait une video seule
        match now {
            Some(mov) if is_live_mov(&mov) => {}
            other => {
                entries[i].live_mov = None;
                if let Some(mov) = other {
                    insert_sorted(entries, mov);
                }
                out.changed = true;
            }
        }
        return;
    }
    if let Some(new) = now {
        insert_sorted(entries, new);
        out.changed = true;
    }
}

/// Remet les appariements Live Photo du `stem` en accord avec `list` : un `.mov` n'est rattache qu'a l'unique image
/// de ce stem, et se detache si une seconde image arrive ; il redevient une entree a part entiere. A l'inverse, la
/// video seule est absorbee quand l'image devient unique (arrivee de l'image, depart de sa jumelle).
fn restitch(entries: &mut Vec<Entry>, stem: &str, out: &mut Reconciled) {
    let images: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].kind != MediaKind::Video && stem_key(&entries[i].path) == stem).collect();
    if let [img] = images[..] {
        if entries[img].live_mov.is_some() {
            return;
        }
        if let Some(j) = entries.iter().position(|e| is_live_mov(e) && stem_key(&e.path) == stem) {
            // la video seule devient la partie animee de la Live Photo
            let mov = entries.remove(j);
            let img = if j < img { img - 1 } else { img };
            entries[img].live_mov = Some(mov.path.clone());
            out.removed.push((j, mov));
            out.changed = true;
        }
        return;
    }
    let detached: Vec<PathBuf> = images.into_iter().filter_map(|i| entries[i].live_mov.take()).collect();
    out.changed |= !detached.is_empty();
    for mov in detached.iter().filter_map(|p| stat_entry(p)) {
        insert_sorted(entries, mov);
    }
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

/// Un `.mov` assez petit pour etre le clip d'une Live Photo (quelques secondes, moins de `LIVE_MOV_MAX`) ; un plus
/// gros est une video autonome.
fn is_live_mov(e: &Entry) -> bool {
    is_mov(e) && e.kind == MediaKind::Video && e.size < LIVE_MOV_MAX
}

/// `entries` est deja trie. Un `.mov` de taille Live Photo est rattache a l'image (hors video) de meme stem, casse
/// ignoree (IMG_0001.HEIC et IMG_0001.MOV chez Apple), si elle est la seule de ce stem : avec IMG_0001.HEIC et
/// IMG_0001.jpg l'appartenance est incertaine, et rattacher le `.mov` le ferait partir avec la mauvaise image a la
/// suppression. L'ordre de la liste reste celui des images.
fn pair_live_photos(entries: Vec<Entry>) -> Vec<Entry> {
    // (index de l'image, nombre d'images) par stem
    let mut image_of_stem: HashMap<String, (usize, usize)> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.kind != MediaKind::Video {
            image_of_stem.entry(stem_key(&e.path)).or_insert((i, 0)).1 += 1;
        }
    }
    let mut live: HashMap<usize, PathBuf> = HashMap::new();
    let mut paired = vec![false; entries.len()];
    for (i, e) in entries.iter().enumerate() {
        if is_live_mov(e)
            && let Some(&(img, 1)) = image_of_stem.get(&stem_key(&e.path))
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

    fn names(list: &[Entry]) -> Vec<String> {
        list.iter().map(|e| file_name(&e.path)).collect()
    }

    /// Change le mtime de `p` (sans attendre une seconde) pour simuler une reecriture.
    fn touch_later(p: &Path) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        let t = f.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(10);
        f.set_modified(t).unwrap();
    }

    #[test]
    fn insert_sorted_keeps_the_natural_order() {
        let dir = TmpDir::new("dir-insert");
        let mut v = Vec::new();
        for name in ["img10.jpg", "img2.jpg", "IMG1.jpg", "img3.jpg"] {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            insert_sorted(&mut v, stat_entry(&p).unwrap());
        }
        assert_eq!(names(&v), ["IMG1.jpg", "img2.jpg", "img3.jpg", "img10.jpg"]);
    }

    #[test]
    fn reconcile_inserts_updates_and_removes_idempotently() {
        let dir = TmpDir::new("dir-reconcile");
        for name in ["a1.jpg", "a3.jpg"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let mut v = list(dir.path()).unwrap();
        let p = |n: &str| dir.path().join(n);

        // nouveau fichier : insere a sa place
        std::fs::write(p("a2.jpg"), b"xy").unwrap();
        let r = reconcile(&mut v, &p("a2.jpg"));
        assert!(r.changed && r.removed.is_empty() && r.updated.is_empty());
        assert_eq!(names(&v), ["a1.jpg", "a2.jpg", "a3.jpg"]);
        assert_eq!(reconcile(&mut v, &p("a2.jpg")), Reconciled::default(), "second appel : rien");

        // modifie : l'ancienne version est rendue, l'entree garde sa place
        let before = v[1].clone();
        touch_later(&p("a2.jpg"));
        let r = reconcile(&mut v, &p("a2.jpg"));
        assert_eq!(r.updated, std::slice::from_ref(&before));
        assert!(r.changed && v[1].mtime_ns > before.mtime_ns);
        assert_eq!(reconcile(&mut v, &p("a2.jpg")), Reconciled::default());

        // disparu : retire, avec son index
        std::fs::remove_file(p("a2.jpg")).unwrap();
        let r = reconcile(&mut v, &p("a2.jpg"));
        assert_eq!(r.removed.len(), 1);
        assert_eq!((r.removed[0].0, file_name(&r.removed[0].1.path)), (1, "a2.jpg".to_string()));
        assert_eq!(names(&v), ["a1.jpg", "a3.jpg"]);
        assert_eq!(reconcile(&mut v, &p("a2.jpg")), Reconciled::default());

        // caches, non medias et absents : ignores
        std::fs::write(p(".h.jpg"), b"x").unwrap();
        std::fs::write(p("n.txt"), b"x").unwrap();
        for n in [".h.jpg", "n.txt", "absent.jpg"] {
            assert_eq!(reconcile(&mut v, &p(n)), Reconciled::default(), "{n}");
        }
        assert_eq!(names(&v), ["a1.jpg", "a3.jpg"]);
    }

    #[test]
    fn reconcile_follows_live_photo_pairs() {
        let dir = TmpDir::new("dir-reconcile-live");
        let p = |n: &str| dir.path().join(n);
        std::fs::write(p("IMG_1.HEIC"), b"x").unwrap();
        let mut v = list(dir.path()).unwrap();

        // le .mov arrive apres l'image : rattache, pas d'entree en plus
        std::fs::write(p("IMG_1.MOV"), b"x").unwrap();
        assert!(reconcile(&mut v, &p("IMG_1.MOV")).changed);
        assert_eq!(names(&v), ["IMG_1.HEIC"]);
        assert_eq!(v[0].live_mov.as_deref(), Some(p("IMG_1.MOV").as_path()));
        assert_eq!(reconcile(&mut v, &p("IMG_1.MOV")), Reconciled::default());

        // le .mov part : l'image reste, sans live_mov
        std::fs::remove_file(p("IMG_1.MOV")).unwrap();
        assert!(reconcile(&mut v, &p("IMG_1.MOV")).changed);
        assert_eq!((names(&v), v[0].live_mov.clone()), (vec!["IMG_1.HEIC".to_string()], None));

        // l'image part avec un .mov toujours present : le .mov redevient une video seule
        std::fs::write(p("IMG_1.MOV"), b"x").unwrap();
        reconcile(&mut v, &p("IMG_1.MOV"));
        std::fs::remove_file(p("IMG_1.HEIC")).unwrap();
        let r = reconcile(&mut v, &p("IMG_1.HEIC"));
        assert_eq!(r.removed.len(), 1);
        assert_eq!(names(&v), ["IMG_1.MOV"]);
        assert_eq!(v[0].kind, MediaKind::Video);

        // l'image revient apres son .mov : la video seule est absorbee, comme le ferait `list`
        std::fs::write(p("IMG_1.HEIC"), b"x").unwrap();
        let r = reconcile(&mut v, &p("IMG_1.HEIC"));
        assert_eq!(r.removed.len(), 1, "la video seule quitte la liste");
        assert_eq!(names(&v), ["IMG_1.HEIC"]);
        assert_eq!(v, list(dir.path()).unwrap(), "meme resultat qu'un nouveau listing");
    }

    #[test]
    fn a_mov_is_paired_only_with_a_unique_image_and_below_the_live_photo_size() {
        let dir = TmpDir::new("dir-live-rules");
        let p = |n: &str| dir.path().join(n);
        for name in ["IMG_1.HEIC", "IMG_1.jpg", "IMG_1.MOV", "vacances.jpg", "vacances.mov", "ok.jpg", "ok.mov"] {
            std::fs::write(p(name), b"x").unwrap();
        }
        // un clip au-dela du seuil : une video autonome, meme avec une image unique de meme nom
        std::fs::File::options().write(true).open(p("vacances.mov")).unwrap().set_len(LIVE_MOV_MAX).unwrap();

        let list = list(dir.path()).unwrap();
        assert_eq!(names(&list), ["IMG_1.HEIC", "IMG_1.jpg", "IMG_1.MOV", "ok.jpg", "vacances.jpg", "vacances.mov"]);
        let by_name = |n: &str| list.iter().find(|e| file_name(&e.path) == n).unwrap();
        for n in ["IMG_1.HEIC", "IMG_1.jpg", "IMG_1.MOV", "vacances.jpg", "vacances.mov"] {
            assert_eq!(by_name(n).live_mov, None, "{n}");
        }
        assert_eq!(by_name("IMG_1.MOV").kind, MediaKind::Video);
        assert_eq!(by_name("ok.jpg").live_mov.as_deref(), Some(p("ok.mov").as_path()));

        // juste au-dessous du seuil : apparie
        std::fs::File::options().write(true).open(p("vacances.mov")).unwrap().set_len(LIVE_MOV_MAX - 1).unwrap();
        let list = super::list(dir.path()).unwrap();
        assert_eq!(names(&list), ["IMG_1.HEIC", "IMG_1.jpg", "IMG_1.MOV", "ok.jpg", "vacances.jpg"]);
    }

    #[test]
    fn reconcile_unpairs_and_repairs_like_list() {
        let dir = TmpDir::new("dir-reconcile-rules");
        let p = |n: &str| dir.path().join(n);
        std::fs::write(p("IMG_1.HEIC"), b"x").unwrap();
        std::fs::write(p("IMG_1.MOV"), b"x").unwrap();
        let mut v = list(dir.path()).unwrap();
        assert_eq!((names(&v), v[0].live_mov.is_some()), (vec!["IMG_1.HEIC".to_string()], true));

        // une seconde image du meme nom : l'appartenance du .mov devient incertaine, il redevient une video
        std::fs::write(p("IMG_1.jpg"), b"x").unwrap();
        assert!(reconcile(&mut v, &p("IMG_1.jpg")).changed);
        assert_eq!(names(&v), ["IMG_1.HEIC", "IMG_1.jpg", "IMG_1.MOV"]);
        assert!(v.iter().all(|e| e.live_mov.is_none()));
        assert_eq!(v, list(dir.path()).unwrap());
        assert_eq!(reconcile(&mut v, &p("IMG_1.jpg")), Reconciled::default());

        // la jumelle part : l'image restante redevient unique et absorbe le .mov
        std::fs::remove_file(p("IMG_1.jpg")).unwrap();
        let r = reconcile(&mut v, &p("IMG_1.jpg"));
        assert!(r.changed);
        assert_eq!(names(&v), ["IMG_1.HEIC"]);
        assert_eq!(v, list(dir.path()).unwrap());

        // le .mov grossit au-dela d'un clip de Live Photo : video autonome
        std::fs::File::options().write(true).open(p("IMG_1.MOV")).unwrap().set_len(LIVE_MOV_MAX).unwrap();
        assert!(reconcile(&mut v, &p("IMG_1.MOV")).changed);
        assert_eq!(names(&v), ["IMG_1.HEIC", "IMG_1.MOV"]);
        assert_eq!(v, list(dir.path()).unwrap());
        assert_eq!(reconcile(&mut v, &p("IMG_1.MOV")), Reconciled::default());

        // et se rattache de nouveau en redevenant petit
        std::fs::File::options().write(true).open(p("IMG_1.MOV")).unwrap().set_len(5).unwrap();
        assert!(reconcile(&mut v, &p("IMG_1.MOV")).changed);
        assert_eq!(names(&v), ["IMG_1.HEIC"]);
        assert_eq!(v, list(dir.path()).unwrap());
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
