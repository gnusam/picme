//! Controller : liste des entrees, image courante, ce qu'il faut decoder, ce qu'il faut afficher. Main thread
//! uniquement, sans GTK : le type de texture est un parametre (`gdk::Texture` dans l'application, un simple couple
//! de dimensions dans les tests) et les pools sont vus a travers le trait `Jobs`.
//!
//! Regles :
//! - le courant est identifie par son chemin, jamais par un index ;
//! - aucune methode ne bloque sur un decode : chacune rend tout de suite ce qu'il faut afficher (`Show`) ;
//! - un resultat n'entre dans le cache que s'il fait partie de ce qui est voulu au moment ou il arrive, et n'est
//!   affiche que s'il appartient au courant et vaut mieux que ce qui est deja affiche de lui ;
//! - le cache garde des `Preview` et des `Fit` par identite de fichier (chemin, mtime, taille), pas par taille de
//!   fenetre : un resize reutilise ce qui est la, et ne redemande un `Fit` que s'il est plus petit que la fenetre ;
//! - un seul slot `Full`, hors cache (le budget du cache est reduit d'autant), libere des qu'on quitte l'image.

use crate::fileops::FileOpsHandle;
use picme_core::cache::LruBytes;
use picme_core::decode::{self, Frame, PREVIEW_SIZE, Target};
use picme_core::dir::{self, Entry};
use picme_core::error::{Error, Result};
use picme_core::media::{self, MediaKind};
use picme_core::pool::{Key, Pool};
use picme_core::thumbs::ThumbStore;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Priorites (la plus petite passe en premier). `Preview` et `Full` ne sont pas dans le meme pool.
pub const PRIO_PREVIEW: u32 = 0;
pub const PRIO_FULL: u32 = 0;
pub const PRIO_FIT: u32 = 1;
/// Voisins : +1 puis -1 puis +2, dans le sens du deplacement.
const NEIGHBOURS: [(u32, isize); 3] = [(2, 1), (3, -1), (4, 2)];

/// Niveau d'une texture, du plus pauvre au plus riche.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Preview,
    Fit,
    Full,
}

impl Level {
    fn of(target: Target) -> Level {
        match target {
            Target::Preview => Level::Preview,
            Target::Fit(..) => Level::Fit,
            Target::Full => Level::Full,
        }
    }
}

/// Version d'un fichier : un resultat d'une autre version (fichier modifie depuis) ne lui correspond pas.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Ident {
    path: PathBuf,
    mtime_ns: i64,
    size: u64,
}

impl Ident {
    fn of(e: &Entry) -> Ident {
        Ident { path: e.path.clone(), mtime_ns: e.mtime_ns, size: e.size }
    }

    fn of_key(k: &Key) -> Ident {
        Ident { path: k.path.clone(), mtime_ns: k.mtime_ns, size: k.size }
    }
}

/// Texture prete a afficher. `w` x `h` : la texture ; `full_w` x `full_h` : l'image entiere (orientee), ce qui
/// fixe la place a l'ecran quel que soit le niveau (un `Preview` s'etire sur la place qu'aura le `Fit`).
#[derive(Debug, Clone)]
pub struct Image<T> {
    pub texture: T,
    pub level: Level,
    pub w: u32,
    pub h: u32,
    pub full_w: u32,
    pub full_h: u32,
}

/// Ce que la vue doit faire apres un appel au controller.
#[derive(Debug)]
pub enum Show<T> {
    /// Rien ne change a l'ecran. Apres une navigation vers une image dont rien n'est encore pret, l'image
    /// precedente reste affichee jusqu'a l'arrivee de la nouvelle (pas d'ecran noir entre deux images).
    Keep,
    Image(Image<T>),
    /// Plus rien a afficher : liste vide, ou video et RAW sans miniature (ils n'ont pas de decode).
    Clear,
    /// Le courant ne peut pas etre decode ; la vue efface l'image et signale le message.
    Failed(String),
}

/// Les pools vus du controller.
pub trait Jobs {
    fn submit(&self, key: Key, priority: u32);
    fn retain(&self, keep: &mut dyn FnMut(&Key) -> bool);
}

impl Jobs for Pool<Key> {
    fn submit(&self, key: Key, priority: u32) {
        Pool::submit(self, key, priority);
    }

    fn retain(&self, keep: &mut dyn FnMut(&Key) -> bool) {
        Pool::retain(self, keep);
    }
}

/// Seuls les types qui ont un decodeur ont un `Fit` et un `Full` ; les autres n'ont que la miniature freedesktop.
fn decodable(kind: MediaKind) -> bool {
    !matches!(kind, MediaKind::Video | MediaKind::Raw)
}

/// Dimensions d'une image `fw` x `fh` dans une boite `bw` x `bh`, jamais agrandie (meme calcul que le decode).
pub(crate) fn fit_dims(fw: u32, fh: u32, bw: u32, bh: u32) -> (u32, u32) {
    if fw <= bw && fh <= bh {
        return (fw, fh);
    }
    let s = (bw as f64 / fw as f64).min(bh as f64 / fh as f64);
    (((fw as f64 * s).round() as u32).clamp(1, bw), ((fh as f64 * s).round() as u32).clamp(1, bh))
}

/// Chemin absolu sans `..` : dossier canonique, nom de fichier tel quel (un lien symbolique se parcourt dans le
/// dossier ou il est, pas dans celui de sa cible).
fn canonical(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (abs.parent().and_then(|d| std::fs::canonicalize(d).ok()), abs.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => abs,
    }
}

/// Une texture `Fit` suffit a la fenetre si elle n'est pas plus petite que ce que la fenetre afficherait (a un pixel
/// d'arrondi pres).
fn fit_is_enough<T>(img: &Image<T>, (vw, vh): (u32, u32)) -> bool {
    let (nw, nh) = fit_dims(img.full_w, img.full_h, vw, vh);
    img.w + 1 >= nw && img.h + 1 >= nh
}

pub struct Controller<T> {
    entries: Vec<Entry>,
    /// Dossier liste (absolu) : les evenements et scans d'un autre dossier sont ignores.
    dir: Option<PathBuf>,
    current: Option<PathBuf>,
    /// Sens du dernier deplacement (+1 ou -1) : les voisins se prechargent dans ce sens d'abord.
    step: isize,
    /// Taille de la vue en device pixels, inconnue tant que la fenetre n'est pas allouee.
    viewport: Option<(u32, u32)>,
    full_on: bool,
    budget: usize,
    cache: LruBytes<(Ident, Level), Image<T>>,
    full: Option<(Ident, Image<T>)>,
    /// Ce que la vue affiche : identite, niveau, largeur de texture.
    shown: Option<(Ident, Level, u32)>,
    /// Ce qui est voulu depuis la derniere mise a jour : seuls ces resultats sont gardes.
    wanted: HashSet<(Ident, Level)>,
    /// Echecs, pour ne pas redemander un decode qui echoue a chaque mise a jour. Message vide : pas d'erreur a
    /// signaler (video ou RAW sans miniature).
    failed: HashMap<(Ident, Level), String>,
    decode: Box<dyn Jobs>,
    thumbs: Box<dyn Jobs>,
    make: Box<dyn Fn(Frame) -> T>,
}

impl<T: Clone> Controller<T> {
    /// `decode` recoit les `Fit` et `Full`, `thumbs` les `Preview` ; `make` transforme une frame en texture.
    pub fn new(budget: usize, decode: Box<dyn Jobs>, thumbs: Box<dyn Jobs>, make: Box<dyn Fn(Frame) -> T>) -> Self {
        Controller {
            entries: Vec::new(),
            dir: None,
            current: None,
            step: 1,
            viewport: None,
            full_on: false,
            budget,
            cache: LruBytes::new(budget),
            full: None,
            shown: None,
            wanted: HashSet::new(),
            failed: HashMap::new(),
            decode,
            thumbs,
            make,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn index(&self) -> Option<usize> {
        let cur = self.current.as_ref()?;
        self.entries.iter().position(|e| &e.path == cur)
    }

    pub fn current(&self) -> Option<&Entry> {
        self.index().map(|i| &self.entries[i])
    }

    /// Ouvre `path`. Hors du dossier courant, la liste devient ce seul fichier en attendant le scan du nouveau
    /// dossier (`set_entries`), que l'appelant lance s'il voit `dir()` changer.
    pub fn open(&mut self, path: &Path) -> Show<T> {
        let path = canonical(path);
        if !self.entries.iter().any(|e| e.path == path) {
            let Some(entry) = dir::stat_entry(&path) else {
                return Show::Failed(format!("{} : introuvable ou pas une image", path.display()));
            };
            let parent = path.parent().map(Path::to_path_buf);
            if parent != self.dir {
                self.entries = vec![entry];
                self.dir = parent;
            } else {
                dir::insert_sorted(&mut self.entries, entry);
            }
        }
        self.set_current(Some(path));
        self.refresh()
    }

    /// Resultat du scan de `dir`. Le courant reste le meme fichier : s'il manque au scan (fichier cache, ou `.mov`
    /// rattache a une image), il est garde a sa place, ou remplace par l'image dont il est le `.mov`.
    pub fn set_entries(&mut self, dir: &Path, entries: Vec<Entry>) -> Show<T> {
        if self.dir.as_deref() != Some(dir) {
            return Show::Keep;
        }
        self.entries = entries;
        if let Some(cur) = self.current.clone()
            && !self.entries.iter().any(|e| e.path == cur)
        {
            if let Some(img) = self.entries.iter().find(|e| e.live_mov.as_ref() == Some(&cur)) {
                let img = img.path.clone();
                self.set_current(Some(img));
            } else if let Some(entry) = dir::stat_entry(&cur) {
                dir::insert_sorted(&mut self.entries, entry);
            } else {
                let first = self.entries.first().map(|e| e.path.clone());
                self.set_current(first);
            }
        }
        self.refresh()
    }

    /// Avance de `delta` entrees (bornees aux extremites, sans boucler).
    pub fn go(&mut self, delta: isize) -> Show<T> {
        let Some(i) = self.index() else { return Show::Keep };
        if delta != 0 {
            self.step = delta.signum();
        }
        let j = (i as isize).saturating_add(delta).clamp(0, self.entries.len() as isize - 1) as usize;
        if j == i {
            return Show::Keep;
        }
        self.set_current(Some(self.entries[j].path.clone()));
        self.refresh()
    }

    /// Nouvelle taille de vue (device pixels). L'appelant applique le debounce apres la premiere.
    pub fn set_viewport(&mut self, w: u32, h: u32) -> Show<T> {
        if w == 0 || h == 0 || self.viewport == Some((w, h)) {
            return Show::Keep;
        }
        self.viewport = Some((w, h));
        self.refresh()
    }

    /// Zoom au-dela du fit : demande le `Full` du courant (et le garde tant qu'on reste sur l'image).
    pub fn set_full(&mut self, on: bool) -> Show<T> {
        if self.full_on == on {
            return Show::Keep;
        }
        self.full_on = on;
        self.refresh()
    }

    /// Re-stat de `path` apres un evenement du dossier (ou une operation de picme). Idempotent.
    pub fn reconcile(&mut self, path: &Path) -> Show<T> {
        if self.dir.is_none() || path.parent() != self.dir.as_deref() {
            return Show::Keep;
        }
        let before = self.index();
        let r = dir::reconcile(&mut self.entries, path);
        if !r.changed {
            return Show::Keep;
        }
        for old in r.updated.iter().chain(r.removed.iter().map(|(_, e)| e)) {
            self.forget(&Ident::of(old));
        }
        if let Some(cur) = self.current.clone()
            && !self.entries.iter().any(|e| e.path == cur)
        {
            let next = if let Some(img) = self.entries.iter().find(|e| e.live_mov.as_ref() == Some(&cur)) {
                Some(img.path.clone())
            } else {
                // l'entree qui prend sa place, ou la derniere s'il etait en fin de liste
                let at = r.removed.iter().find(|(_, e)| e.path == cur).map(|(i, _)| *i).or(before).unwrap_or(0);
                self.entries.get(at.min(self.entries.len().saturating_sub(1))).map(|e| e.path.clone())
            };
            self.set_current(next);
        }
        self.refresh()
    }

    /// Resultat d'un pool.
    pub fn on_result(&mut self, key: Key, result: Result<Frame>) -> Show<T> {
        let id = Ident::of_key(&key);
        let level = Level::of(key.target);
        if !self.wanted.contains(&(id.clone(), level)) {
            // plus voulu (navigation, resize, fichier modifie) : jete, sans texture ni place dans le cache
            return Show::Keep;
        }
        let is_current = self.current().is_some_and(|e| Ident::of(e) == id);
        let frame = match result {
            Ok(frame) => frame,
            Err(e) => {
                let quiet = matches!(e, Error::Unsupported(_)) && !media::kind_from_path(&id.path).is_some_and(decodable);
                let msg = if quiet { String::new() } else { format!("{} : {e}", id.path.display()) };
                self.failed.insert((id.clone(), level), msg.clone());
                if is_current && !self.shows(&id) {
                    self.shown = None;
                    return if msg.is_empty() { Show::Clear } else { Show::Failed(msg) };
                }
                return Show::Keep;
            }
        };
        if level == Level::Fit
            && let Some(have) = self.cache.peek(&(id.clone(), Level::Fit))
            && have.w >= frame.w
        {
            // un Fit au moins aussi grand est deja la (resultat d'une ancienne taille de fenetre)
            return Show::Keep;
        }
        let bytes = frame.bytes();
        let (w, h, full_w, full_h) = (frame.w, frame.h, frame.full_w, frame.full_h);
        let img = Image { texture: (self.make)(frame), level, w, h, full_w, full_h };
        match level {
            Level::Full => {
                self.full = Some((id.clone(), img.clone()));
                self.cache.set_budget(self.budget.saturating_sub(bytes));
            }
            _ => {
                self.cache.insert((id.clone(), level), img.clone(), bytes);
            }
        }
        if is_current && self.better_than_shown(&id, &img) {
            self.shown = Some((id, level, img.w));
            return Show::Image(img);
        }
        Show::Keep
    }

    fn shows(&self, id: &Ident) -> bool {
        self.shown.as_ref().is_some_and(|(s, _, _)| s == id)
    }

    fn better_than_shown(&self, id: &Ident, img: &Image<T>) -> bool {
        match &self.shown {
            Some((s, level, w)) if s == id => img.level > *level || (img.level == *level && img.w > *w),
            _ => true,
        }
    }

    fn set_current(&mut self, path: Option<PathBuf>) {
        if self.current != path {
            self.current = path;
            self.full_on = false;
            self.release_full();
        }
    }

    fn release_full(&mut self) {
        if self.full.take().is_some() {
            self.cache.set_budget(self.budget);
        }
    }

    /// Oublie tout ce qui concerne une version de fichier perimee (modifiee ou disparue).
    fn forget(&mut self, id: &Ident) {
        for level in [Level::Preview, Level::Fit] {
            self.cache.remove(&(id.clone(), level));
        }
        self.failed.retain(|(f, _), _| f != id);
        if self.full.as_ref().is_some_and(|(f, _)| f == id) {
            self.release_full();
        }
    }

    /// Recalcule ce qui est voulu, met les pools a jour et rend le meilleur affichage disponible du courant.
    fn refresh(&mut self) -> Show<T> {
        self.wanted.clear();
        let mut jobs: Vec<(Key, u32, bool)> = Vec::new(); // (cle, priorite, pool decode ?)
        let Some(i) = self.index() else {
            self.decode.retain(&mut |_| false);
            self.thumbs.retain(&mut |k| k.target != Target::Preview);
            self.shown = None;
            return Show::Clear;
        };
        let cur = self.entries[i].clone();
        let id = Ident::of(&cur);
        self.cache.pin((id.clone(), Level::Fit));

        let has_full = self.full.as_ref().is_some_and(|(f, _)| *f == id);
        let has_fit = self.cache.contains(&(id.clone(), Level::Fit));
        if decodable(cur.kind) {
            if self.full_on && !has_full {
                self.want(&cur, Target::Full, PRIO_FULL, true, &mut jobs);
            }
            if let Some(vp) = self.viewport
                && !self.fit_enough(&id, vp)
            {
                self.want(&cur, Target::Fit(vp.0, vp.1), PRIO_FIT, true, &mut jobs);
            }
        }
        if !has_full && !has_fit && !self.cache.contains(&(id.clone(), Level::Preview)) {
            self.want(&cur, Target::Preview, PRIO_PREVIEW, false, &mut jobs);
        }
        if let Some(vp) = self.viewport {
            for (prio, off) in NEIGHBOURS {
                let Some(n) = i.checked_add_signed(off * self.step).and_then(|j| self.entries.get(j)) else { continue };
                if decodable(n.kind) && !self.fit_enough(&Ident::of(n), vp) {
                    let n = n.clone();
                    self.want(&n, Target::Fit(vp.0, vp.1), prio, true, &mut jobs);
                }
            }
        }

        let keys: HashSet<&Key> = jobs.iter().map(|(k, _, _)| k).collect();
        self.decode.retain(&mut |k| keys.contains(k));
        // le pool thumbs servira aussi le filmstrip : le controller n'y retire que ses Preview
        self.thumbs.retain(&mut |k| k.target != Target::Preview || keys.contains(k));
        for (key, prio, on_decode) in jobs {
            if on_decode { self.decode.submit(key, prio) } else { self.thumbs.submit(key, prio) }
        }

        let best = if has_full {
            self.full.as_ref().map(|(_, img)| img.clone())
        } else {
            [Level::Fit, Level::Preview].into_iter().find_map(|l| self.cache.get(&(id.clone(), l)).cloned())
        };
        match best {
            Some(img) if self.better_than_shown(&id, &img) => {
                self.shown = Some((id, img.level, img.w));
                Show::Image(img)
            }
            Some(_) => Show::Keep,
            None if self.shows(&id) => Show::Keep,
            None => {
                let failure = [Level::Preview, Level::Fit].into_iter().find_map(|l| self.failed.get(&(id.clone(), l)));
                match failure {
                    Some(msg) if !msg.is_empty() => {
                        self.shown = None;
                        Show::Failed(msg.clone())
                    }
                    // video ou RAW : l'image precedente ne doit pas rester sous son nom
                    _ if failure.is_some() || !decodable(cur.kind) => {
                        self.shown = None;
                        Show::Clear
                    }
                    _ => Show::Keep,
                }
            }
        }
    }

    fn fit_enough(&self, id: &Ident, vp: (u32, u32)) -> bool {
        self.cache.peek(&(id.clone(), Level::Fit)).is_some_and(|img| fit_is_enough(img, vp))
    }

    /// Ajoute `target` de `entry` a ce qui est voulu, et a soumettre sauf echec deja connu.
    fn want(&mut self, entry: &Entry, target: Target, prio: u32, on_decode: bool, jobs: &mut Vec<(Key, u32, bool)>) {
        let id = Ident::of(entry);
        let level = Level::of(target);
        if self.failed.contains_key(&(id.clone(), level)) {
            return;
        }
        self.wanted.insert((id, level));
        jobs.push((Key::of(entry, target), prio, on_decode));
    }
}

/// Travail du pool `decode` : `Fit` et `Full`, plafond de `Full` a la moitie du budget du cache.
pub fn decode_work(budget: usize) -> impl Fn(&Key) -> Result<Frame> + Send + Sync + 'static {
    move |key: &Key| decode::decode_capped(&key.path, key.target, budget / 2)
}

/// Travail du pool `thumbs` : miniature freedesktop si elle est a jour, sinon decode `Preview`. Un `Preview` decode
/// qui a la taille d'une miniature `large` (256 px de grand cote, ou l'image entiere si elle est plus petite) part
/// au thread d'ecriture ; la thumbnail embarquee d'un HEIF, plus petite, n'est jamais ecrite.
pub fn thumb_work(store: Option<ThumbStore>, writer: FileOpsHandle) -> impl Fn(&Key) -> Result<Frame> + Send + Sync + 'static {
    move |key: &Key| {
        let kind = media::kind_from_path(&key.path).ok_or_else(|| Error::Unsupported(key.path.display().to_string()))?;
        let entry = Entry { path: key.path.clone(), kind, live_mov: None, mtime_ns: key.mtime_ns, size: key.size };
        if let Some(frame) = store.as_ref().and_then(|s| s.read(&entry)) {
            return Ok(frame);
        }
        let frame = decode::decode(&key.path, Target::Preview)?;
        let large = frame.w.max(frame.h) == PREVIEW_SIZE || (frame.w, frame.h) == (frame.full_w, frame.full_h);
        if store.is_some() && large {
            writer.store_thumb(entry, frame.clone());
        }
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileops::FileOps;
    use crate::testutil::TmpDir;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// File de jobs factice : ce qui a ete soumis et pas retire, avec sa priorite.
    #[derive(Default)]
    struct Fake(RefCell<Vec<(Key, u32)>>);

    struct FakeJobs(Rc<Fake>);

    impl Jobs for FakeJobs {
        fn submit(&self, key: Key, priority: u32) {
            let mut q = self.0.0.borrow_mut();
            match q.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = priority,
                None => q.push((key, priority)),
            }
        }

        fn retain(&self, keep: &mut dyn FnMut(&Key) -> bool) {
            self.0.0.borrow_mut().retain(|(k, _)| keep(k));
        }
    }

    impl Fake {
        /// (nom de fichier, cible, priorite), par priorite.
        fn jobs(&self) -> Vec<(String, Target, u32)> {
            let mut v: Vec<_> =
                self.0.borrow().iter().map(|(k, p)| (k.path.file_name().unwrap().to_string_lossy().into_owned(), k.target, *p)).collect();
            v.sort_by_key(|j| j.2);
            v
        }

        fn take(&self, name: &str, level: Level) -> Key {
            let mut q = self.0.borrow_mut();
            let i = q.iter().position(|(k, _)| k.path.ends_with(name) && Level::of(k.target) == level).unwrap_or_else(|| panic!("pas de job {name} {level:?}"));
            q.remove(i).0
        }
    }

    /// Texture de test : ses dimensions.
    type Tex = (u32, u32);

    struct Rig {
        c: Controller<Tex>,
        decode: Rc<Fake>,
        thumbs: Rc<Fake>,
        dir: TmpDir,
    }

    const FULL: (u32, u32) = (4000, 3000);

    fn frame(w: u32, h: u32) -> Frame {
        Frame { rgba: vec![0; w as usize * h as usize * 4], w, h, full_w: FULL.0, full_h: FULL.1 }
    }

    /// Dossier de `n` fichiers vides `i00.jpg`, `i01.jpg`... listes, et le courant sur `i{cur}`.
    fn rig(name: &str, n: usize, cur: usize, budget: usize) -> Rig {
        let dir = TmpDir::new(name);
        for i in 0..n {
            std::fs::write(dir.path().join(format!("i{i:02}.jpg")), b"x").unwrap();
        }
        let (decode, thumbs) = (Rc::new(Fake::default()), Rc::new(Fake::default()));
        let mut c = Controller::new(
            budget,
            Box::new(FakeJobs(decode.clone())),
            Box::new(FakeJobs(thumbs.clone())),
            Box::new(|f: Frame| (f.w, f.h)),
        );
        c.open(&dir.path().join(format!("i{cur:02}.jpg")));
        let list = dir::list(dir.path()).unwrap();
        c.set_entries(dir.path(), list);
        Rig { c, decode, thumbs, dir }
    }

    fn name(c: &Controller<Tex>) -> String {
        c.current().unwrap().path.file_name().unwrap().to_string_lossy().into_owned()
    }

    fn shown<T: std::fmt::Debug>(s: Show<T>) -> (Level, u32, u32) {
        match s {
            Show::Image(img) => (img.level, img.w, img.h),
            other => panic!("attendu une image, recu {other:?}"),
        }
    }

    fn fit(name: &str, p: u32) -> (String, Target, u32) {
        (name.to_string(), Target::Fit(800, 600), p)
    }

    #[test]
    fn preview_first_then_fit_and_neighbours_in_the_move_direction() {
        let mut r = rig("ctl-wanted", 10, 3, 256 << 20);
        // taille de fenetre encore inconnue : seulement le Preview du courant
        assert_eq!(r.thumbs.jobs(), [("i03.jpg".to_string(), Target::Preview, 0)]);
        assert!(r.decode.jobs().is_empty());

        assert!(matches!(r.c.set_viewport(800, 600), Show::Keep));
        assert_eq!(r.decode.jobs(), [fit("i03.jpg", 1), fit("i04.jpg", 2), fit("i02.jpg", 3), fit("i05.jpg", 4)]);

        // vers la gauche : le courant change, les voisins suivent le sens, les jobs perimes sont retires
        assert!(matches!(r.c.go(-1), Show::Keep), "rien de pret : l'image precedente reste affichee");
        assert_eq!(name(&r.c), "i02.jpg");
        assert_eq!(r.decode.jobs(), [fit("i02.jpg", 1), fit("i01.jpg", 2), fit("i03.jpg", 3), fit("i00.jpg", 4)]);
        assert_eq!(r.thumbs.jobs(), [("i02.jpg".to_string(), Target::Preview, 0)]);

        // bornes : pas de voisin hors liste, et pas de bouclage
        r.c.go(-10);
        assert_eq!(name(&r.c), "i00.jpg");
        assert_eq!(r.decode.jobs(), [fit("i00.jpg", 1), fit("i01.jpg", 3)]);
        assert!(matches!(r.c.go(-1), Show::Keep));
        assert_eq!(name(&r.c), "i00.jpg");
    }

    #[test]
    fn stale_results_are_neither_shown_nor_cached() {
        let mut r = rig("ctl-stale", 10, 3, 256 << 20);
        r.c.set_viewport(800, 600);
        let old_preview = r.thumbs.take("i03.jpg", Level::Preview);
        let old_fit = r.decode.take("i03.jpg", Level::Fit);
        let far_fit = r.decode.take("i05.jpg", Level::Fit);
        let mut stale = far_fit.clone();
        r.c.go(1);
        r.c.go(1);
        assert_eq!(name(&r.c), "i05.jpg");

        // resultats arrives apres deux navigations : i03 n'est ni le courant ni un voisin voulu (6, 4, 7)
        assert!(matches!(r.c.on_result(old_preview.clone(), Ok(frame(256, 192))), Show::Keep));
        assert!(matches!(r.c.on_result(old_fit.clone(), Ok(frame(800, 600))), Show::Keep));
        assert!(!r.c.cache.contains(&(Ident::of_key(&old_fit), Level::Fit)), "Fit d'une image plus voulue : jete");
        assert!(!r.c.cache.contains(&(Ident::of_key(&old_preview), Level::Preview)), "Preview d'un ancien courant : jete");
        assert_eq!(r.c.cache.used_bytes(), 0);

        // le Fit de i05 soumis avant la navigation reste valable : affiche
        assert_eq!(shown(r.c.on_result(far_fit, Ok(frame(800, 600)))), (Level::Fit, 800, 600));

        // resultat d'une version perimee du fichier courant (mtime different) : jete
        stale.mtime_ns -= 1;
        assert!(matches!(r.c.on_result(stale.clone(), Ok(frame(800, 600))), Show::Keep));
        assert!(!r.c.cache.contains(&(Ident::of_key(&stale), Level::Fit)));
    }

    #[test]
    fn a_result_outside_the_wanted_set_never_enters_the_cache() {
        let mut r = rig("ctl-unwanted", 10, 3, 256 << 20);
        r.c.set_viewport(800, 600);
        let fit6 = r.decode.take("i05.jpg", Level::Fit); // +2
        r.c.go(-1); // voisins de i02 : 1, 3, 0 -> i05 n'est plus voulu
        assert!(matches!(r.c.on_result(fit6.clone(), Ok(frame(800, 600))), Show::Keep));
        assert!(!r.c.cache.contains(&(Ident::of_key(&fit6), Level::Fit)));
        assert_eq!(r.c.cache.used_bytes(), 0);
    }

    #[test]
    fn levels_only_go_up_for_the_current_image() {
        let mut r = rig("ctl-levels", 5, 2, 256 << 20);
        r.c.set_viewport(800, 600);
        let preview = r.thumbs.take("i02.jpg", Level::Preview);
        let fit = r.decode.take("i02.jpg", Level::Fit);
        assert_eq!(shown(r.c.on_result(preview.clone(), Ok(frame(256, 192)))), (Level::Preview, 256, 192));
        assert_eq!(shown(r.c.on_result(fit, Ok(frame(800, 600)))), (Level::Fit, 800, 600));
        // un Preview en retard (pool thumbs plus lent) ne remplace pas le Fit
        assert!(matches!(r.c.on_result(preview, Ok(frame(256, 192))), Show::Keep));
    }

    #[test]
    fn a_cached_fit_shows_at_once_and_is_not_requested_again() {
        let mut r = rig("ctl-hit", 10, 3, 256 << 20);
        r.c.set_viewport(800, 600);
        let next = r.decode.take("i04.jpg", Level::Fit);
        assert!(matches!(r.c.on_result(next, Ok(frame(800, 600))), Show::Keep), "voisin : en cache, pas affiche");
        assert_eq!(shown(r.c.go(1)), (Level::Fit, 800, 600));
        assert!(!r.decode.jobs().iter().any(|j| j.0 == "i04.jpg"), "{:?}", r.decode.jobs());
        assert!(r.thumbs.jobs().is_empty(), "pas de Preview quand le Fit est la");
        // retour en arriere : i03 n'a rien en cache, l'image de i04 reste a l'ecran
        assert!(matches!(r.c.go(-1), Show::Keep));
        assert_eq!(r.thumbs.jobs(), [("i03.jpg".to_string(), Target::Preview, 0)]);
    }

    #[test]
    fn resize_reuses_the_fit_and_asks_again_only_when_it_is_too_small() {
        let mut r = rig("ctl-resize", 5, 2, 256 << 20);
        r.c.set_viewport(800, 600);
        let fit = r.decode.take("i02.jpg", Level::Fit);
        shown(r.c.on_result(fit.clone(), Ok(frame(800, 600))));
        // plus petite : la texture suffit, rien a redemander pour le courant
        r.c.set_viewport(400, 300);
        assert!(!r.decode.jobs().iter().any(|j| j.0 == "i02.jpg"));
        // plus grande : nouveau Fit, l'ancienne texture reste affichee en attendant
        assert!(matches!(r.c.set_viewport(1600, 1200), Show::Keep));
        let big = r.decode.take("i02.jpg", Level::Fit);
        assert_eq!(big.target, Target::Fit(1600, 1200));
        assert_eq!(shown(r.c.on_result(big, Ok(frame(1600, 1200)))), (Level::Fit, 1600, 1200));
        // l'ancien Fit (800x600) qui arrive en retard ne remplace ni l'affichage ni le cache
        assert!(matches!(r.c.on_result(fit, Ok(frame(800, 600))), Show::Keep));
        let id = Ident::of(r.c.current().unwrap());
        assert_eq!(r.c.cache.peek(&(id, Level::Fit)).unwrap().w, 1600);
    }

    #[test]
    fn a_fit_of_the_old_size_is_shown_while_the_new_one_is_decoded() {
        // deux tailles coup sur coup (taille par defaut puis tuile du WM) : le premier Fit n'est pas jete
        let mut r = rig("ctl-two-sizes", 5, 2, 256 << 20);
        r.c.set_viewport(800, 600);
        let first = r.decode.take("i02.jpg", Level::Fit);
        r.c.set_viewport(1600, 1200);
        assert_eq!(shown(r.c.on_result(first, Ok(frame(800, 600)))), (Level::Fit, 800, 600));
        assert!(r.decode.jobs().iter().any(|j| j.0 == "i02.jpg" && j.1 == Target::Fit(1600, 1200)));
    }

    #[test]
    fn full_only_on_zoom_in_one_slot_released_when_leaving() {
        let budget = 100 << 20;
        let mut r = rig("ctl-full", 5, 2, budget);
        r.c.set_viewport(800, 600);
        assert!(!r.decode.jobs().iter().any(|j| j.1 == Target::Full), "pas de Full sans zoom");
        r.c.set_full(true);
        assert_eq!(r.decode.jobs()[0], ("i02.jpg".to_string(), Target::Full, 0));
        let full = r.decode.take("i02.jpg", Level::Full);
        assert_eq!(shown(r.c.on_result(full, Ok(frame(4000, 3000)))), (Level::Full, 4000, 3000));
        assert_eq!(r.c.cache.budget(), budget - 4000 * 3000 * 4, "le slot Full est decompte du budget");
        // le Fit qui arrive ensuite ne fait pas redescendre l'affichage
        let fit = r.decode.take("i02.jpg", Level::Fit);
        assert!(matches!(r.c.on_result(fit, Ok(frame(800, 600))), Show::Keep));
        // on quitte l'image : slot libere, budget rendu, pas de Full pour la suivante
        r.c.go(1);
        assert!(r.c.full.is_none());
        assert_eq!(r.c.cache.budget(), budget);
        assert!(!r.decode.jobs().iter().any(|j| j.1 == Target::Full));
    }

    #[test]
    fn current_fit_is_pinned_and_the_cache_stays_within_budget() {
        // budget de deux Fit : le courant reste, les voisins tournent
        let one = 800 * 600 * 4;
        let mut r = rig("ctl-budget", 20, 0, 2 * one);
        r.c.set_viewport(800, 600);
        let cur = r.decode.take("i00.jpg", Level::Fit);
        shown(r.c.on_result(cur.clone(), Ok(frame(800, 600))));
        let mut asked_again = 0;
        for round in 0..4 {
            // tout ce qui est demande arrive : i01 et i02, qui s'evincent l'un l'autre
            let jobs: Vec<Key> = r.decode.0.borrow_mut().drain(..).map(|(k, _)| k).collect();
            if round > 0 {
                asked_again += jobs.len();
            }
            for k in jobs {
                r.c.on_result(k, Ok(frame(800, 600)));
            }
            assert!(r.c.cache.used_bytes() <= 2 * one);
            assert!(r.c.cache.contains(&(Ident::of_key(&cur), Level::Fit)), "le courant n'est jamais evince");
            // mise a jour suivante (meme Fit a un pixel pres) : seul le voisin evince est redemande
            r.c.set_viewport(800, 601 + round);
            assert!(r.decode.jobs().iter().all(|j| j.0 != "i00.jpg"), "{:?}", r.decode.jobs());
        }
        assert_eq!(asked_again, 3);
    }

    #[test]
    fn a_failed_decode_is_reported_and_not_retried() {
        let mut r = rig("ctl-failed", 5, 2, 256 << 20);
        let preview = r.thumbs.take("i02.jpg", Level::Preview);
        match r.c.on_result(preview, Err(Error::Decode("corrompu".into()))) {
            Show::Failed(m) => assert!(m.contains("corrompu"), "{m}"),
            other => panic!("{other:?}"),
        }
        r.c.set_viewport(800, 600);
        assert!(r.thumbs.jobs().is_empty(), "Preview en echec : pas redemande");
        // on revient sur l'image : l'echec est rappele sans nouveau decode
        r.c.go(1);
        assert!(matches!(r.c.go(-1), Show::Failed(_)));
        assert!(r.thumbs.jobs().iter().all(|j| j.0 != "i02.jpg"));
    }

    #[test]
    fn videos_clear_the_view_and_only_ask_for_a_cached_thumbnail() {
        let mut r = rig("ctl-video", 3, 0, 256 << 20);
        std::fs::write(r.dir.path().join("i01b.mov"), b"x").unwrap();
        let list = dir::list(r.dir.path()).unwrap();
        r.c.set_entries(r.dir.path(), list);
        r.c.set_viewport(800, 600);
        assert!(!r.decode.jobs().iter().any(|j| j.0 == "i01b.mov"), "pas de Fit pour une video voisine");
        r.c.go(1);
        assert_eq!(name(&r.c), "i01.jpg");
        assert!(matches!(r.c.go(1), Show::Clear), "video sans miniature : l'image precedente ne reste pas");
        assert_eq!(name(&r.c), "i01b.mov");
        assert!(!r.decode.jobs().iter().any(|j| j.0 == "i01b.mov"));
        let thumb = r.thumbs.take("i01b.mov", Level::Preview);
        assert!(matches!(r.c.on_result(thumb, Err(Error::Unsupported("video".into()))), Show::Clear));
    }

    #[test]
    fn reconcile_moves_off_a_deleted_current_and_reloads_a_modified_one() {
        let mut r = rig("ctl-reconcile", 4, 1, 256 << 20);
        r.c.set_viewport(800, 600);
        let fit = r.decode.take("i01.jpg", Level::Fit);
        shown(r.c.on_result(fit.clone(), Ok(frame(800, 600))));

        // modifie : l'ancienne version est oubliee, la nouvelle demandee, l'ancienne image reste a l'ecran
        let p1 = r.dir.path().join("i01.jpg");
        std::fs::write(&p1, b"plus long").unwrap();
        assert!(matches!(r.c.reconcile(&p1), Show::Keep));
        assert!(!r.c.cache.contains(&(Ident::of_key(&fit), Level::Fit)));
        let fresh = r.decode.take("i01.jpg", Level::Fit);
        assert_ne!(fresh.size, fit.size);
        assert_eq!(r.thumbs.jobs()[0].0, "i01.jpg");
        assert!(matches!(r.c.reconcile(&p1), Show::Keep), "idempotent");

        // supprime : le courant passe a celui qui prend sa place
        std::fs::remove_file(&p1).unwrap();
        r.c.reconcile(&p1);
        assert_eq!(name(&r.c), "i02.jpg");
        // en fin de liste : au precedent
        r.c.go(10);
        let p3 = r.dir.path().join("i03.jpg");
        std::fs::remove_file(&p3).unwrap();
        r.c.reconcile(&p3);
        assert_eq!(name(&r.c), "i02.jpg");
        // nouveau fichier : insere a sa place, le courant ne bouge pas
        let p = r.dir.path().join("i02a.jpg");
        std::fs::write(&p, b"x").unwrap();
        r.c.reconcile(&p);
        assert_eq!(name(&r.c), "i02.jpg");
        assert_eq!(r.c.entries().len(), 3);
        // hors du dossier : ignore
        assert!(matches!(r.c.reconcile(Path::new("/ailleurs/x.jpg")), Show::Keep));
        // tout supprime : vue videe
        for n in ["i00.jpg", "i02.jpg", "i02a.jpg"] {
            let p = r.dir.path().join(n);
            std::fs::remove_file(&p).unwrap();
            r.c.reconcile(&p);
        }
        assert!(r.c.current().is_none());
        assert!(r.decode.jobs().is_empty() && r.thumbs.jobs().is_empty());
    }

    #[test]
    fn scan_without_the_current_keeps_it_and_relative_paths_are_absolute() {
        let dir = TmpDir::new("ctl-scan");
        for n in ["a.jpg", ".h.jpg", "c.jpg", "IMG_1.HEIC", "IMG_1.MOV"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let (decode, thumbs) = (Rc::new(Fake::default()), Rc::new(Fake::default()));
        let mut c: Controller<Tex> =
            Controller::new(1 << 20, Box::new(FakeJobs(decode)), Box::new(FakeJobs(thumbs.clone())), Box::new(|f: Frame| (f.w, f.h)));

        // chemin relatif au dossier courant des tests (le dossier du crate)
        let cwd = std::env::current_dir().unwrap();
        let rel: PathBuf = cwd.components().skip(1).map(|_| "..").collect::<PathBuf>().join(dir.path().strip_prefix("/").unwrap()).join(".h.jpg");
        assert!(rel.is_relative());
        c.open(&rel);
        assert_eq!(c.dir(), Some(dir.path()));
        assert_eq!(c.current().unwrap().path, dir.path().join(".h.jpg"));
        assert_eq!(thumbs.jobs().len(), 1, "Preview demande avant le scan");

        // le scan ignore les fichiers caches : le courant est garde a sa place triee
        c.set_entries(dir.path(), dir::list(dir.path()).unwrap());
        assert_eq!(c.current().unwrap().path, dir.path().join(".h.jpg"));
        assert_eq!(c.entries().len(), 4);

        // un .mov apparie ouvert directement : le courant devient l'image de la Live Photo
        c.open(&dir.path().join("IMG_1.MOV"));
        c.set_entries(dir.path(), dir::list(dir.path()).unwrap());
        assert_eq!(c.current().unwrap().path, dir.path().join("IMG_1.HEIC"));

        // scan d'un autre dossier (perime) : ignore
        assert!(matches!(c.set_entries(Path::new("/ailleurs"), Vec::new()), Show::Keep));
        assert_eq!(c.entries().len(), 3, "le fichier cache n'est plus courant : plus garde");
        // fichier absent
        assert!(matches!(c.open(&dir.path().join("absent.jpg")), Show::Failed(_)));
    }

    #[test]
    fn thumb_work_reads_the_cache_and_writes_only_large_previews() {
        let dir = TmpDir::new("ctl-thumbwork");
        let root = dir.path().join("thumbnails");
        for n in ["a.jpg", "k_thumb.heic"] {
            std::fs::copy(picme_core::fixtures_dir().join(n), dir.path().join(n)).unwrap();
        }
        let store = ThumbStore::new(&root);
        let ops = FileOps::new(Some(store.clone()));
        let work = thumb_work(Some(store.clone()), ops.handle());
        let key = |n: &str| Key::of(&dir::stat_entry(&dir.path().join(n)).unwrap(), Target::Preview);

        let f = work(&key("a.jpg")).unwrap();
        assert_eq!((f.w, f.h, f.full_w, f.full_h), (256, 192, 640, 480));
        // thumbnail embarquee 128x96 : affichee, jamais ecrite comme miniature large
        let h = work(&key("k_thumb.heic")).unwrap();
        assert_eq!((h.w, h.h), (128, 96));
        ops.finish();

        let entry = |n: &str| dir::stat_entry(&dir.path().join(n)).unwrap();
        let t = store.read(&entry("a.jpg")).expect("miniature large ecrite");
        assert_eq!((t.w, t.h, t.full_w, t.full_h), (256, 192, 640, 480));
        assert!(store.read(&entry("k_thumb.heic")).is_none());
        // seconde lecture : servie par la miniature (meme resultat)
        let ops = FileOps::new(Some(store.clone()));
        let again = thumb_work(Some(store), ops.handle())(&key("a.jpg")).unwrap();
        assert_eq!(again.rgba, t.rgba);
    }
}
