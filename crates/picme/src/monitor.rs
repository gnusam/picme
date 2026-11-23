//! Surveillance du dossier affiche (gio::FileMonitor). Les evenements sont ramenes a des chemins a re-statter
//! (`Controller::reconcile`), regroupes par un debounce de 150 ms : une copie qui emet cinq evenements par fichier
//! ne reconcilie qu'une fois chaque chemin. Les fichiers caches (dont les temporaires de gio) sont ignores.

use crate::chrome::OneShot;
use gtk4::gio::{self, FileMonitorEvent, FileMonitorFlags, prelude::*};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Les chemins a re-statter pour un evenement. `Changed` est ecarte : un fichier en cours d'ecriture en emet a
/// chaque bloc, `ChangesDoneHint` annonce la fin. Un renommage touche l'ancien nom et le nouveau.
pub fn paths_of(event: FileMonitorEvent, file: &Path, other: Option<&Path>) -> Vec<PathBuf> {
    let wanted: &[Option<&Path>] = match event {
        FileMonitorEvent::Created | FileMonitorEvent::Deleted | FileMonitorEvent::ChangesDoneHint | FileMonitorEvent::MovedIn | FileMonitorEvent::MovedOut => {
            &[Some(file), None]
        }
        FileMonitorEvent::Renamed => &[Some(file), other],
        _ => return Vec::new(),
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for p in wanted.iter().flatten() {
        if !is_hidden(p) && !out.iter().any(|o| o == p) {
            out.push(p.to_path_buf());
        }
    }
    out
}

fn is_hidden(p: &Path) -> bool {
    p.file_name().is_none_or(|n| n.as_encoded_bytes().starts_with(b"."))
}

/// Garde la surveillance en vie ; la lacher l'arrete.
pub struct DirWatch {
    monitor: gio::FileMonitor,
    _timer: Rc<OneShot>,
}

impl DirWatch {
    /// `on_paths` recoit, `DEBOUNCE` apres le dernier evenement, les chemins touches (sans doublon, dans l'ordre
    /// d'arrivee). Appele sur le main thread.
    pub fn new(dir: &Path, on_paths: impl Fn(Vec<PathBuf>) + 'static) -> Result<DirWatch, gtk4::glib::Error> {
        let monitor = gio::File::for_path(dir).monitor_directory(FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)?;
        let pending: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        let timer = Rc::new(OneShot::default());
        let on_paths = Rc::new(on_paths);
        monitor.connect_changed({
            let timer = timer.clone();
            move |_, file, other, event| {
                let (Some(file), other) = (file.path(), other.and_then(|o| o.path())) else { return };
                let paths = paths_of(event, &file, other.as_deref());
                if paths.is_empty() {
                    return;
                }
                {
                    let mut p = pending.borrow_mut();
                    for path in paths {
                        if !p.contains(&path) {
                            p.push(path);
                        }
                    }
                }
                let pending = pending.clone();
                let on_paths = on_paths.clone();
                timer.arm(DEBOUNCE, move || {
                    let batch = std::mem::take(&mut *pending.borrow_mut());
                    on_paths(batch);
                });
            }
        });
        Ok(DirWatch { monitor, _timer: timer })
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        self.monitor.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn created_deleted_and_done_writing_touch_the_file() {
        for ev in [FileMonitorEvent::Created, FileMonitorEvent::Deleted, FileMonitorEvent::ChangesDoneHint, FileMonitorEvent::MovedIn, FileMonitorEvent::MovedOut] {
            assert_eq!(paths_of(ev, Path::new("/d/a.jpg"), None), vec![p("/d/a.jpg")], "{ev:?}");
        }
    }

    #[test]
    fn a_rename_touches_both_names() {
        assert_eq!(paths_of(FileMonitorEvent::Renamed, Path::new("/d/a.jpg"), Some(Path::new("/d/b.jpg"))), vec![p("/d/a.jpg"), p("/d/b.jpg")]);
        // l'atomic write (temporaire cache -> nom final) ne retient que le nom final
        assert_eq!(paths_of(FileMonitorEvent::Renamed, Path::new("/d/.goutputstream-X1"), Some(Path::new("/d/b.jpg"))), vec![p("/d/b.jpg")]);
    }

    #[test]
    fn partial_writes_hidden_files_and_attributes_are_ignored() {
        assert!(paths_of(FileMonitorEvent::Changed, Path::new("/d/a.jpg"), None).is_empty());
        assert!(paths_of(FileMonitorEvent::AttributeChanged, Path::new("/d/a.jpg"), None).is_empty());
        assert!(paths_of(FileMonitorEvent::PreUnmount, Path::new("/d/a.jpg"), None).is_empty());
        assert!(paths_of(FileMonitorEvent::Created, Path::new("/d/.hidden.jpg"), None).is_empty());
    }
}
