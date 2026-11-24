//! Ecritures disque : un seul thread, dans l'ordre d'arrivee, pour que deux operations sur un meme fichier ne se
//! croisent jamais (R trois fois puis Suppr : c'est le fichier tourne trois fois qui part a la corbeille).
//!
//! Les operations sur les fichiers (rotation, crop, corbeille, suppression) rendent un [`Done`] au main thread, qui
//! remet la liste en accord avec le disque (`reconcile`) et affiche le message. Apres chacune, les miniatures
//! `normal` et `large` du fichier sont effacees : leur `Thumb::MTime` est en secondes et ne verrait pas une reecriture
//! dans la meme seconde. Les tailles `x-large` et `xx-large`, ecrites par d'autres programmes, ne sont pas touchees
//! (elles sont ignorees a la lecture des que le mtime differe). La corbeille ne se replie jamais sur une suppression.
//!
//! Annulation : Suppr et Maj+Suppr passent toutes deux par la corbeille, et Ctrl+Z remet le dernier retrait a sa place
//! pendant `UNDO_WINDOW`. Maj+Suppr vide ensuite la corbeille de ce qu'il a retire (a l'expiration, ou a l'arret du
//! thread : quitter rend la suppression definitive). Sur un systeme de fichiers sans corbeille, Maj+Suppr supprime
//! tout de suite, sans annulation possible.
//!
//! Les ecritures de miniatures sont facultatives : au-dela de `MAX_THUMB_BACKLOG` en attente, les suivantes sont
//! jetees (elles seront refaites a la prochaine ouverture) pour ne pas retenir des frames en memoire pendant un
//! defilement rapide. Une miniature dont le fichier a change depuis le decode (rotation passee avant elle) est jetee.

use gtk4::gio::{self, prelude::FileExt};
use picme_core::decode::Frame;
use picme_core::dir::{self, Entry};
use picme_core::edit::crop::{self, Cropped, Frac};
use picme_core::edit::rotate::{self, Saved};
use picme_core::error::{Error, Result};
use picme_core::orient::Orientation;
use picme_core::thumbs::ThumbStore;
use crate::trashbin::{self, Trashed};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAX_THUMB_BACKLOG: usize = 16;

/// Delai pendant lequel Ctrl+Z remet un fichier retire.
pub const UNDO_WINDOW: Duration = Duration::from_secs(30);

/// Mise a la corbeille d'un fichier : [`gio_trash`] dans l'application, remplacable dans les tests.
pub type TrashFn = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

enum Op {
    Rotate { path: PathBuf, turn: Orientation },
    Crop { path: PathBuf, rect: Frac, replace: bool },
    Trash { paths: Vec<PathBuf> },
    Delete { paths: Vec<PathBuf> },
    Undo,
}

/// Un retrait annulable : ce qui est a la corbeille, et s'il faut l'y vider a l'expiration (Maj+Suppr).
struct Undo {
    items: Vec<Trashed>,
    purge: bool,
    deadline: Instant,
}

enum Msg {
    StoreThumb { entry: Entry, frame: Frame },
    Op(Op),
    Stop,
}

/// Compte rendu d'une operation sur les fichiers, pour le main thread.
#[derive(Debug)]
pub enum Done {
    /// Rotation enregistree dans `path`, ou l'erreur (`Unsupported` : format dont seule la vue peut tourner).
    Rotated { path: PathBuf, result: Result<Saved> },
    /// Crop de `path` : la copie ecrite, ou l'original remplace.
    Cropped { path: PathBuf, result: Result<Cropped> },
    /// `paths` : tout ce qui etait demande (l'image puis son `.mov`) ; `error` : le premier echec, apres lequel rien
    /// n'a ete tente.
    /// `undo` : Ctrl+Z peut encore le remettre.
    Removed { paths: Vec<PathBuf>, trash: bool, undo: bool, error: Option<(PathBuf, Error)> },
    /// Ctrl+Z : `paths` remis a leur place (vide : rien a annuler) ; `error` : le premier echec.
    Restored { paths: Vec<PathBuf>, error: Option<(PathBuf, Error)> },
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Le nom d'un retrait : l'image, et son `.mov` s'il y en a un.
fn what(paths: &[PathBuf]) -> String {
    match paths {
        [img, _mov] => format!("{} et son .mov", name(img)),
        _ => paths.first().map(|p| name(p)).unwrap_or_default(),
    }
}

impl Done {
    /// Vrai si l'operation a fait tout ce qui etait demande.
    pub fn ok(&self) -> bool {
        match self {
            Done::Rotated { result, .. } => result.is_ok(),
            Done::Cropped { result, .. } => result.is_ok(),
            Done::Removed { error, .. } | Done::Restored { error, .. } => error.is_none(),
        }
    }

    /// Le message de l'OSD.
    pub fn message(&self) -> String {
        match self {
            Done::Rotated { result: Ok(Saved::Pixels), .. } => "rotation enregistrée".into(),
            Done::Rotated { result: Ok(Saved::Tag), .. } => "rotation enregistrée (tag Orientation)".into(),
            Done::Rotated { result: Err(Error::Unsupported(_)), .. } => "rotation non enregistrée".into(),
            Done::Rotated { path, result: Err(e) } => format!("rotation non enregistrée : {} : {e}", name(path)),
            Done::Cropped { result: Ok(c), .. } if c.replaced => format!("{} recadrée, original à la corbeille", name(&c.path)),
            Done::Cropped { result: Ok(c), .. } => {
                format!("copie recadrée : {}{}", name(&c.path), if c.lossless { "" } else { " (JPEG ré-encodé)" })
            }
            Done::Cropped { path, result: Err(e) } => format!("recadrage impossible : {} : {e}", name(path)),
            Done::Removed { paths, trash, undo, error: None } => {
                let what = what(paths);
                let hint = if *undo { " (Ctrl+Z annule)" } else { "" };
                if *trash { format!("{what} à la corbeille{hint}") } else { format!("{what} supprimé{hint}") }
            }
            Done::Restored { paths, error: None } if paths.is_empty() => "rien à annuler".into(),
            Done::Restored { paths, error: None } => format!("{} remis en place", what(paths)),
            Done::Restored { error: Some((p, e)), .. } => format!("annulation impossible : {} : {e}", name(p)),
            Done::Removed { trash, error: Some((p, e)), .. } => {
                format!("{} impossible : {} : {e}", if *trash { "corbeille" } else { "suppression" }, name(p))
            }
        }
    }
}

/// Corbeille freedesktop par GIO (`$XDG_DATA_HOME/Trash`, ou `.Trash-<uid>` a la racine d'un autre systeme de
/// fichiers). Echoue sur un systeme de fichiers sans corbeille : l'appelant ne se replie jamais sur une suppression.
pub fn gio_trash(path: &Path) -> Result<()> {
    gio::File::for_path(path).trash(None::<&gio::Cancellable>).map_err(|e| Error::Io(std::io::Error::other(e)))
}

fn gio_delete(path: &Path) -> Result<()> {
    gio::File::for_path(path).delete(None::<&gio::Cancellable>).map_err(|e| Error::Io(std::io::Error::other(e)))
}

/// Soumission depuis n'importe quel thread (le pool thumbs en tient un).
#[derive(Clone)]
pub struct FileOpsHandle {
    tx: mpsc::Sender<Msg>,
    thumbs_pending: Arc<AtomicUsize>,
}

impl FileOpsHandle {
    /// Ecrit les miniatures `large` et `normal` de `entry`. Faux si elles sont jetees (file pleine, ou thread arrete).
    pub fn store_thumb(&self, entry: Entry, frame: Frame) -> bool {
        if self.thumbs_pending.fetch_add(1, Ordering::AcqRel) >= MAX_THUMB_BACKLOG {
            self.thumbs_pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        if self.tx.send(Msg::StoreThumb { entry, frame }).is_err() {
            self.thumbs_pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// Enregistre la rotation `turn` de l'image `path` (`Rotate90` pour R, `Rotate270` pour Maj+R). Les operations
    /// ci-dessous rendent faux si le thread est arrete (rien n'est fait, aucun `Done` ne viendra).
    pub fn rotate(&self, path: PathBuf, turn: Orientation) -> bool {
        self.send(Op::Rotate { path, turn })
    }

    /// Recadre `path` : copie `<stem>-crop.<ext>`, ou remplacement de l'original (mis d'abord a la corbeille).
    pub fn crop(&self, path: PathBuf, rect: Frac, replace: bool) -> bool {
        self.send(Op::Crop { path, rect, replace })
    }

    /// Met `paths` a la corbeille, dans l'ordre (l'image puis son `.mov`), en s'arretant au premier echec.
    pub fn trash(&self, paths: Vec<PathBuf>) -> bool {
        self.send(Op::Trash { paths })
    }

    /// Supprime `paths`, dans l'ordre, en s'arretant au premier echec : par la corbeille, videe apres `UNDO_WINDOW`.
    pub fn delete(&self, paths: Vec<PathBuf>) -> bool {
        self.send(Op::Delete { paths })
    }

    /// Ctrl+Z : remet a sa place le dernier retrait encore dans `UNDO_WINDOW`.
    pub fn undo(&self) -> bool {
        self.send(Op::Undo)
    }

    fn send(&self, op: Op) -> bool {
        self.tx.send(Msg::Op(op)).is_ok()
    }
}

pub struct FileOps {
    handle: FileOpsHandle,
    thread: Option<JoinHandle<()>>,
}

impl FileOps {
    /// `thumbs` : dossier des miniatures freedesktop, `None` s'il n'y en a pas (les ecritures sont alors ignorees) ;
    /// `done` : comptes rendus des operations sur les fichiers.
    pub fn new(thumbs: Option<ThumbStore>, done: async_channel::Sender<Done>) -> Self {
        Self::with_trash(thumbs, done, Arc::new(gio_trash))
    }

    /// [`FileOps::new`] avec une autre corbeille (tests).
    pub fn with_trash(thumbs: Option<ThumbStore>, done: async_channel::Sender<Done>, trash: TrashFn) -> Self {
        Self::with_trash_and_window(thumbs, done, trash, UNDO_WINDOW)
    }

    /// [`FileOps::new`] avec un autre delai d'annulation (tests).
    pub fn with_window(thumbs: Option<ThumbStore>, done: async_channel::Sender<Done>, window: Duration) -> Self {
        Self::with_trash_and_window(thumbs, done, Arc::new(gio_trash), window)
    }

    fn with_trash_and_window(thumbs: Option<ThumbStore>, done: async_channel::Sender<Done>, trash: TrashFn, window: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let thumbs_pending = Arc::new(AtomicUsize::new(0));
        let pending = thumbs_pending.clone();
        let thread = std::thread::Builder::new()
            .name("picme-fileops".into())
            .spawn(move || {
                let mut undo: Vec<Undo> = Vec::new();
                loop {
                    expire(&mut undo);
                    // la plus ancienne echeance est en tete : les delais sont tous les memes
                    let msg = match undo.first() {
                        None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        Some(u) => rx.recv_timeout(u.deadline.saturating_duration_since(Instant::now())),
                    };
                    match msg {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(Msg::StoreThumb { entry, frame }) => {
                            let fresh = dir::stat_entry(&entry.path).is_some_and(|e| (e.mtime_ns, e.size) == (entry.mtime_ns, entry.size));
                            if let Some(store) = &thumbs
                                && fresh
                                && let Err(e) = store.store_standard(&entry, frame)
                            {
                                eprintln!("picme : miniature de {} : {e}", entry.path.display());
                            }
                            pending.fetch_sub(1, Ordering::AcqRel);
                        }
                        Ok(Msg::Op(op)) => {
                            // une panique dans une operation ne doit pas tuer le thread : les suivantes (et leur
                            // compte rendu, que le main thread attend) passeraient sous silence
                            let failed = failure(&op);
                            let result = catch_unwind(AssertUnwindSafe(|| run(op, thumbs.as_ref(), &*trash, &mut undo, window)));
                            let _ = done.send_blocking(result.unwrap_or_else(|p| {
                                let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned());
                                failed(Error::Io(std::io::Error::other(format!("panique : {}", msg.unwrap_or_default()))))
                            }));
                        }
                        Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                // quitter rend definitif ce que Maj+Suppr avait retire
                for u in undo.drain(..) {
                    finish(u);
                }
            })
            .expect("thread fileops");
        FileOps { handle: FileOpsHandle { tx, thumbs_pending }, thread: Some(thread) }
    }

    pub fn handle(&self) -> FileOpsHandle {
        self.handle.clone()
    }

    /// Termine les operations deja soumises puis arrete le thread (aussi fait au drop).
    pub fn finish(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.handle.tx.send(Msg::Stop);
            let _ = thread.join();
        }
    }
}

impl Drop for FileOps {
    fn drop(&mut self) {
        self.stop();
    }
}
/// Le compte rendu d'echec d'une operation, a rendre avec l'erreur si elle a panique : `run` consomme l'operation, le
/// chemin est donc retenu avant.
fn failure(op: &Op) -> Box<dyn FnOnce(Error) -> Done> {
    match op {
        Op::Rotate { path, .. } => {
            let path = path.clone();
            Box::new(move |e| Done::Rotated { path, result: Err(e) })
        }
        Op::Crop { path, .. } => {
            let path = path.clone();
            Box::new(move |e| Done::Cropped { path, result: Err(e) })
        }
        Op::Trash { paths } | Op::Delete { paths } => {
            let (paths, trash) = (paths.clone(), matches!(op, Op::Trash { .. }));
            Box::new(move |e| {
                let first = paths.first().cloned().unwrap_or_default();
                Done::Removed { paths, trash, undo: false, error: Some((first, e)) }
            })
        }
        Op::Undo => Box::new(|e| Done::Restored { paths: Vec::new(), error: Some((PathBuf::new(), e)) }),
    }
}

/// Vide la corbeille de ce que Maj+Suppr y avait mis, quand son delai est fini.
fn finish(u: Undo) {
    if u.purge {
        for t in &u.items {
            if let Err(e) = trashbin::purge(t) {
                eprintln!("picme : corbeille : {} : {e}", t.orig.display());
            }
        }
    }
}

/// Les retraits dont le delai est passe ne sont plus annulables.
fn expire(undo: &mut Vec<Undo>) {
    let now = Instant::now();
    let keep = undo.iter().position(|u| u.deadline > now).unwrap_or(undo.len());
    for u in undo.drain(..keep) {
        finish(u);
    }
}

/// Une operation sur les fichiers, puis l'effacement des miniatures `normal` et `large` de ce qui a change.
fn run(op: Op, thumbs: Option<&ThumbStore>, trash: &dyn Fn(&Path) -> Result<()>, undo: &mut Vec<Undo>, window: Duration) -> Done {
    let forget = |p: &Path| {
        if let Some(store) = thumbs {
            store.forget(p);
        }
    };
    match op {
        Op::Rotate { path, turn } => {
            let result = rotate::rotate(&path, turn);
            if result.is_ok() {
                forget(&path);
            }
            Done::Rotated { path, result }
        }
        Op::Crop { path, rect, replace } => {
            let result = crop::crop(&path, rect, replace.then_some(trash));
            if let Ok(c) = &result
                && c.replaced
            {
                forget(&c.path);
            }
            Done::Cropped { path, result }
        }
        Op::Trash { paths } => remove(paths, false, trash, forget, undo, window),
        Op::Delete { paths } => remove(paths, true, trash, forget, undo, window),
        Op::Undo => restore(undo),
    }
}

/// `purge` : Maj+Suppr, qui vide la corbeille a l'expiration et se replie sur une suppression immediate si le
/// systeme de fichiers n'a pas de corbeille ; Suppr n'a jamais ce repli.
fn remove(paths: Vec<PathBuf>, purge: bool, trash: &dyn Fn(&Path) -> Result<()>, forget: impl Fn(&Path), undo: &mut Vec<Undo>, window: Duration) -> Done {
    let mut error = None;
    let mut items = Vec::new();
    for p in &paths {
        match trash(p) {
            Ok(()) => items.extend(trashbin::locate(p)),
            Err(_) if purge => {
                if let Err(e) = gio_delete(p) {
                    error = Some((p.clone(), e));
                    break;
                }
            }
            Err(e) => {
                error = Some((p.clone(), e));
                break;
            }
        }
        forget(p);
    }
    let undoable = !items.is_empty();
    if undoable {
        undo.push(Undo { items, purge, deadline: Instant::now() + window });
    }
    Done::Removed { paths, trash: !purge, undo: undoable, error }
}

/// Ctrl+Z : remet a sa place le retrait le plus recent. Ce qui n'a pas pu l'etre reste a la corbeille.
fn restore(undo: &mut Vec<Undo>) -> Done {
    let Some(u) = undo.pop() else { return Done::Restored { paths: Vec::new(), error: None } };
    let (mut paths, mut error) = (Vec::new(), None);
    for t in &u.items {
        match trashbin::restore(t) {
            Ok(()) => paths.push(t.orig.clone()),
            Err(e) => {
                error.get_or_insert((t.orig.clone(), Error::Io(e)));
            }
        }
    }
    Done::Restored { paths, error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use picme_core::thumbs::ThumbSize;
    use std::sync::Mutex;

    fn entry(tmp: &TmpDir, i: usize) -> Entry {
        let p = tmp.path().join(format!("{i:03}.jpg"));
        std::fs::copy(picme_core::fixtures_dir().join("a.jpg"), &p).unwrap();
        dir::stat_entry(&p).unwrap()
    }

    fn frame() -> Frame {
        Frame { rgba: vec![128; 256 * 192 * 4], w: 256, h: 192, full_w: 640, full_h: 480, low_res: false, partial: false }
    }

    #[test]
    fn writes_in_order_and_finish_waits_for_them() {
        let tmp = TmpDir::new("fileops-order");
        let store = ThumbStore::new(tmp.path().join("thumbnails"));
        let ops = FileOps::new(Some(store.clone()), async_channel::unbounded().0);
        let entries: Vec<Entry> = (0..MAX_THUMB_BACKLOG).map(|i| entry(&tmp, i)).collect();
        let handle = ops.handle();
        for e in &entries {
            assert!(handle.store_thumb(e.clone(), frame()));
        }
        ops.finish();
        for e in &entries {
            assert!(store.read(e, 256).is_some(), "{}", e.path.display());
            assert!(store.read(e, 128).is_some_and(|f| f.w == 128), "normal : {}", e.path.display());
        }
        // thread arrete : la soumission echoue sans bloquer ni paniquer
        assert!(!handle.store_thumb(entries[0].clone(), frame()));
    }

    #[test]
    fn thumb_writes_beyond_the_backlog_are_dropped() {
        let tmp = TmpDir::new("fileops-backlog");
        let ops = FileOps::new(None, async_channel::unbounded().0);
        let handle = ops.handle();
        // le thread est retenu tant que la file n'est pas pleine : on remplit le compteur a la main
        handle.thumbs_pending.store(MAX_THUMB_BACKLOG, Ordering::Release);
        assert!(!handle.store_thumb(entry(&tmp, 0), frame()));
        assert_eq!(handle.thumbs_pending.load(Ordering::Acquire), MAX_THUMB_BACKLOG);
        handle.thumbs_pending.store(0, Ordering::Release);
        assert!(handle.store_thumb(entry(&tmp, 1), frame()));
        drop(ops);
        assert_eq!(handle.thumbs_pending.load(Ordering::Acquire), 0, "drop attend les ecritures soumises");
    }

    #[test]
    fn a_thumbnail_of_a_file_changed_since_its_decode_is_dropped() {
        let tmp = TmpDir::new("fileops-stale-thumb");
        let store = ThumbStore::new(tmp.path().join("thumbnails"));
        let ops = FileOps::new(Some(store.clone()), async_channel::unbounded().0);
        let e = entry(&tmp, 0);
        std::fs::write(&e.path, b"autre contenu").unwrap();
        assert!(ops.handle().store_thumb(e.clone(), frame()));
        ops.finish();
        assert!(!store.path_of(&e, ThumbSize::Large).exists() && !store.path_of(&e, ThumbSize::Normal).exists());
    }

    /// Thread d'ecriture dont la corbeille note les chemins (et echoue sur ceux de `refuse`), miniatures dans `tmp`.
    fn rig(tmp: &TmpDir, refuse: &'static str) -> (FileOps, async_channel::Receiver<Done>, ThumbStore, Arc<Mutex<Vec<PathBuf>>>) {
        let store = ThumbStore::new(tmp.path().join("thumbnails"));
        let (tx, rx) = async_channel::unbounded();
        let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let trash: TrashFn = Arc::new({
            let seen = seen.clone();
            move |p: &Path| {
                if !refuse.is_empty() && p.ends_with(refuse) {
                    return Err(Error::Io(std::io::Error::other("pas de corbeille ici")));
                }
                seen.lock().unwrap().push(p.to_path_buf());
                std::fs::remove_file(p)?;
                Ok(())
            }
        });
        (FileOps::with_trash(Some(store.clone()), tx, trash), rx, store, seen)
    }

    /// Miniatures picme de `e`, comme apres un premier affichage.
    fn with_thumbs(store: &ThumbStore, e: &Entry) {
        store.store_standard(e, frame()).unwrap();
        assert!(store.path_of(e, ThumbSize::Large).exists());
    }

    #[test]
    fn rotate_rewrites_the_file_then_forgets_its_thumbnails_and_reports() {
        let tmp = TmpDir::new("fileops-rotate");
        let (ops, rx, store, _) = rig(&tmp, "");
        let e = entry(&tmp, 0);
        with_thumbs(&store, &e);
        assert!(ops.handle().rotate(e.path.clone(), Orientation::Rotate90));
        let done = rx.recv_blocking().unwrap();
        assert!(matches!(&done, Done::Rotated { path, result: Ok(Saved::Pixels) } if *path == e.path), "{done:?}");
        assert_eq!(done.message(), "rotation enregistrée");
        let after = dir::stat_entry(&e.path).unwrap();
        assert_ne!((after.mtime_ns, after.size), (e.mtime_ns, e.size));
        assert!(!store.path_of(&e, ThumbSize::Large).exists() && !store.path_of(&e, ThumbSize::Normal).exists());

        // format sans rotation enregistrable : fichier intact, vue seule
        let webp = tmp.path().join("w.webp");
        std::fs::copy(picme_core::fixtures_dir().join("f.webp"), &webp).unwrap();
        let before = std::fs::read(&webp).unwrap();
        ops.handle().rotate(webp.clone(), Orientation::Rotate270);
        let done = rx.recv_blocking().unwrap();
        assert!(!done.ok());
        assert_eq!(done.message(), "rotation non enregistrée");
        assert_eq!(std::fs::read(&webp).unwrap(), before);
    }

    #[test]
    fn trash_takes_the_image_then_its_mov_and_never_falls_back_to_delete() {
        let tmp = TmpDir::new("fileops-trash");
        let (ops, rx, store, seen) = rig(&tmp, "refuse.jpg");
        let e = entry(&tmp, 0);
        with_thumbs(&store, &e);
        let mov = tmp.path().join("000.mov");
        std::fs::write(&mov, b"video").unwrap();
        ops.handle().trash(vec![e.path.clone(), mov.clone()]);
        let done = rx.recv_blocking().unwrap();
        assert!(done.ok(), "{done:?}");
        assert_eq!(done.message(), "000.jpg et son .mov à la corbeille");
        assert_eq!(*seen.lock().unwrap(), [e.path.clone(), mov.clone()]);
        assert!(!store.path_of(&e, ThumbSize::Large).exists());

        // corbeille refusee : l'image reste, son .mov n'est pas tente, rien n'est supprime a la place
        let img = tmp.path().join("refuse.jpg");
        std::fs::write(&img, b"image").unwrap();
        std::fs::write(&mov, b"video").unwrap();
        ops.handle().trash(vec![img.clone(), mov.clone()]);
        let done = rx.recv_blocking().unwrap();
        assert!(matches!(&done, Done::Removed { trash: true, error: Some((p, _)), .. } if *p == img), "{done:?}");
        assert!(done.message().starts_with("corbeille impossible : refuse.jpg : "), "{}", done.message());
        assert!(img.exists() && mov.exists());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn delete_removes_for_good_in_order_and_stops_at_the_first_failure() {
        let tmp = TmpDir::new("fileops-delete");
        let (ops, rx, store, _) = rig(&tmp, "");
        let e = entry(&tmp, 0);
        with_thumbs(&store, &e);
        let mov = tmp.path().join("000.mov");
        std::fs::write(&mov, b"video").unwrap();
        ops.handle().delete(vec![e.path.clone(), mov.clone()]);
        let done = rx.recv_blocking().unwrap();
        assert!(done.ok(), "{done:?}");
        assert!(!e.path.exists() && !mov.exists());
        assert!(!store.path_of(&e, ThumbSize::Large).exists() && !store.path_of(&e, ThumbSize::Normal).exists());

        let gone = tmp.path().join("absent.jpg");
        let other = tmp.path().join("autre.jpg");
        std::fs::write(&other, b"x").unwrap();
        ops.handle().delete(vec![gone.clone(), other.clone()]);
        let done = rx.recv_blocking().unwrap();
        assert!(matches!(&done, Done::Removed { trash: false, error: Some((p, _)), .. } if *p == gone), "{done:?}");
        assert!(other.exists(), "rien n'est tente apres le premier echec");
    }

    #[test]
    fn crop_copy_and_crop_replace_go_through_the_thread_and_its_trash() {
        let tmp = TmpDir::new("fileops-crop");
        let (ops, rx, store, seen) = rig(&tmp, "");
        let e = entry(&tmp, 0);
        let rect = Frac { x: 0.25, y: 0.25, w: 0.5, h: 0.5 };
        ops.handle().crop(e.path.clone(), rect, false);
        let done = rx.recv_blocking().unwrap();
        let copy = tmp.path().join("000-crop.jpg");
        assert!(matches!(&done, Done::Cropped { result: Ok(c), .. } if c.path == copy && !c.replaced && c.lossless), "{done:?}");
        assert_eq!(done.message(), "copie recadrée : 000-crop.jpg");
        assert!(seen.lock().unwrap().is_empty());

        with_thumbs(&store, &e);
        ops.handle().crop(e.path.clone(), rect, true);
        let done = rx.recv_blocking().unwrap();
        assert!(matches!(&done, Done::Cropped { result: Ok(c), .. } if c.path == e.path && c.replaced), "{done:?}");
        assert_eq!(*seen.lock().unwrap(), std::slice::from_ref(&e.path), "original a la corbeille avant d'etre remplace");
        assert!(!store.path_of(&e, ThumbSize::Large).exists());
        assert!(e.path.exists() && copy.exists());
    }

    #[test]
    fn a_panicking_operation_is_reported_as_an_error_and_the_thread_survives() {
        let tmp = TmpDir::new("fileops-panic");
        let (tx, rx) = async_channel::unbounded();
        // corbeille injectee : panique sur "panique.jpg", supprime le reste
        let trash: TrashFn = Arc::new(|p: &Path| {
            if p.ends_with("panique.jpg") {
                panic!("corbeille en panne");
            }
            std::fs::remove_file(p)?;
            Ok(())
        });
        let ops = FileOps::with_trash(None, tx, trash);
        let boom = tmp.path().join("panique.jpg");
        let mov = tmp.path().join("panique.mov");
        std::fs::write(&boom, b"image").unwrap();
        std::fs::write(&mov, b"video").unwrap();
        assert!(ops.handle().trash(vec![boom.clone(), mov.clone()]));
        let done = rx.recv_blocking().unwrap();
        assert!(!done.ok(), "{done:?}");
        assert!(matches!(&done, Done::Removed { trash: true, paths, error: Some((p, _)), .. } if *p == boom && paths.len() == 2), "{done:?}");
        assert!(done.message().contains("corbeille en panne"), "{}", done.message());
        assert!(boom.exists() && mov.exists());

        // le thread a survecu : l'operation suivante est faite et rendue
        let next = tmp.path().join("suivant.jpg");
        std::fs::write(&next, b"x").unwrap();
        assert!(ops.handle().trash(vec![next.clone()]));
        let done = rx.recv_blocking().unwrap();
        assert!(done.ok(), "{done:?}");
        assert!(!next.exists());
    }
}
