//! Ecritures disque : un seul thread, dans l'ordre d'arrivee, pour que deux operations sur un meme fichier ne se
//! croisent jamais (les operations du bloc 5, rotation et suppression, viendront s'ajouter a `Op`).
//!
//! Les ecritures de miniatures sont facultatives : au-dela de `MAX_THUMB_BACKLOG` en attente, les suivantes sont
//! jetees (elles seront refaites a la prochaine ouverture) pour ne pas retenir des frames en memoire pendant un
//! defilement rapide.

use picme_core::decode::Frame;
use picme_core::dir::Entry;
use picme_core::thumbs::ThumbStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

pub const MAX_THUMB_BACKLOG: usize = 16;

enum Op {
    StoreThumb { entry: Entry, frame: Frame },
}

enum Msg {
    Op(Op),
    Stop,
}

/// Soumission depuis n'importe quel thread (le pool thumbs en tient un).
#[derive(Clone)]
pub struct FileOpsHandle {
    tx: mpsc::Sender<Msg>,
    thumbs_pending: Arc<AtomicUsize>,
}

impl FileOpsHandle {
    /// Ecrit la miniature `large` de `entry`. Faux si elle est jetee (file pleine, ou thread arrete).
    pub fn store_thumb(&self, entry: Entry, frame: Frame) -> bool {
        if self.thumbs_pending.fetch_add(1, Ordering::AcqRel) >= MAX_THUMB_BACKLOG {
            self.thumbs_pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        if self.tx.send(Msg::Op(Op::StoreThumb { entry, frame })).is_err() {
            self.thumbs_pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }
}

pub struct FileOps {
    handle: FileOpsHandle,
    thread: Option<JoinHandle<()>>,
}

impl FileOps {
    /// `thumbs` : dossier des miniatures freedesktop, `None` s'il n'y en a pas (les ecritures sont alors ignorees).
    pub fn new(thumbs: Option<ThumbStore>) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let thumbs_pending = Arc::new(AtomicUsize::new(0));
        let pending = thumbs_pending.clone();
        let thread = std::thread::Builder::new()
            .name("picme-fileops".into())
            .spawn(move || {
                while let Ok(Msg::Op(op)) = rx.recv() {
                    match op {
                        Op::StoreThumb { entry, frame } => {
                            if let Some(store) = &thumbs
                                && let Err(e) = store.store(&entry, &frame)
                            {
                                eprintln!("picme : miniature de {} : {e}", entry.path.display());
                            }
                            pending.fetch_sub(1, Ordering::AcqRel);
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use picme_core::dir;

    fn entry(tmp: &TmpDir, i: usize) -> Entry {
        let p = tmp.path().join(format!("{i:03}.jpg"));
        std::fs::copy(picme_core::fixtures_dir().join("a.jpg"), &p).unwrap();
        dir::stat_entry(&p).unwrap()
    }

    fn frame() -> Frame {
        Frame { rgba: vec![128; 256 * 192 * 4], w: 256, h: 192, full_w: 640, full_h: 480 }
    }

    #[test]
    fn writes_in_order_and_finish_waits_for_them() {
        let tmp = TmpDir::new("fileops-order");
        let store = ThumbStore::new(tmp.path().join("thumbnails"));
        let ops = FileOps::new(Some(store.clone()));
        let entries: Vec<Entry> = (0..MAX_THUMB_BACKLOG).map(|i| entry(&tmp, i)).collect();
        let handle = ops.handle();
        for e in &entries {
            assert!(handle.store_thumb(e.clone(), frame()));
        }
        ops.finish();
        for e in &entries {
            assert!(store.read(e).is_some(), "{}", e.path.display());
        }
        // thread arrete : la soumission echoue sans bloquer ni paniquer
        assert!(!handle.store_thumb(entries[0].clone(), frame()));
    }

    #[test]
    fn thumb_writes_beyond_the_backlog_are_dropped() {
        let tmp = TmpDir::new("fileops-backlog");
        let ops = FileOps::new(None);
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
}
