//! Ecritures disque : un seul thread, dans l'ordre d'arrivee, pour que deux operations sur un meme fichier ne se
//! croisent jamais (R trois fois puis Suppr : c'est le fichier tourne trois fois qui part a la corbeille).
//!
//! Les operations sur les fichiers (rotation, crop, corbeille, suppression) rendent un [`Done`] au main thread, qui
//! remet la liste en accord avec le disque (`reconcile`) et affiche le message. Apres chacune, les miniatures
//! `normal` et `large` du fichier sont effacees : leur `Thumb::MTime` est en secondes et ne verrait pas une reecriture
//! dans la meme seconde. Les tailles `x-large` et `xx-large`, ecrites par d'autres programmes, ne sont pas touchees
//! (elles sont ignorees a la lecture des que le mtime differe). La corbeille ne se replie jamais sur une suppression.
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

pub const MAX_THUMB_BACKLOG: usize = 16;

/// Mise a la corbeille d'un fichier : [`gio_trash`] dans l'application, remplacable dans les tests.
pub type TrashFn = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

enum Op {
    Rotate { path: PathBuf, turn: Orientation },
    Crop { path: PathBuf, rect: Frac, replace: bool },
    Trash { paths: Vec<PathBuf> },
    Delete { paths: Vec<PathBuf> },
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
    Removed { paths: Vec<PathBuf>, trash: bool, error: Option<(PathBuf, Error)> },
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl Done {
    /// Vrai si l'operation a fait tout ce qui etait demande.
    pub fn ok(&self) -> bool {
        match self {
            Done::Rotated { result, .. } => result.is_ok(),
            Done::Cropped { result, .. } => result.is_ok(),
            Done::Removed { error, .. } => error.is_none(),
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
            Done::Removed { paths, trash, error: None } => {
                let what = match paths.as_slice() {
                    [img, _mov] => format!("{} et son .mov", name(img)),
                    _ => paths.first().map(|p| name(p)).unwrap_or_default(),
                };
                if *trash { format!("{what} à la corbeille") } else { format!("{what} supprimé") }
            }
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

    /// Supprime `paths` definitivement, dans l'ordre, en s'arretant au premier echec.
    pub fn delete(&self, paths: Vec<PathBuf>) -> bool {
        self.send(Op::Delete { paths })
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
        let (tx, rx) = mpsc::channel::<Msg>();
        let thumbs_pending = Arc::new(AtomicUsize::new(0));
        let pending = thumbs_pending.clone();
        let thread = std::thread::Builder::new()
            .name("picme-fileops".into())
            .spawn(move || {
                loop {
                    match rx.recv() {
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
                            let _ = done.send_blocking(run(op, thumbs.as_ref(), &*trash));
                        }
                        Ok(Msg::Stop) | Err(_) => break,
                    }
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


/// Une operation sur les fichiers, puis l'effacement des miniatures `normal` et `large` de ce qui a change.
fn run(op: Op, thumbs: Option<&ThumbStore>, trash: &dyn Fn(&Path) -> Result<()>) -> Done {
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
        Op::Trash { paths } => remove(paths, true, trash, forget),
        Op::Delete { paths } => remove(paths, false, &gio_delete, forget),
    }
}

fn remove(paths: Vec<PathBuf>, trash: bool, op: &dyn Fn(&Path) -> Result<()>, forget: impl Fn(&Path)) -> Done {
    let mut error = None;
    for p in &paths {
        if let Err(e) = op(p) {
            error = Some((p.clone(), e));
            break;
        }
        forget(p);
    }
    Done::Removed { paths, trash, error }
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
        let (ops, rx, store, seen) = rig(&tmp, "");
        let e = entry(&tmp, 0);
        with_thumbs(&store, &e);
        let mov = tmp.path().join("000.mov");
        std::fs::write(&mov, b"video").unwrap();
        ops.handle().delete(vec![e.path.clone(), mov.clone()]);
        let done = rx.recv_blocking().unwrap();
        assert!(done.ok(), "{done:?}");
        assert!(!e.path.exists() && !mov.exists());
        assert!(seen.lock().unwrap().is_empty(), "pas par la corbeille");
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
}
