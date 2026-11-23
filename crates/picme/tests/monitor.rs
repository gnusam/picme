//! `DirWatch` sur un vrai dossier : un fichier ajoute, renomme ou supprime par un autre programme est signale une
//! fois, apres le debounce, sans fenetre ni GTK (la boucle gio suffit).

use gtk4::glib;
use picme::monitor::{DEBOUNCE, DirWatch};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp").join(format!("monitor-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p.canonicalize().unwrap())
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Le contexte glib par defaut n'appartient qu'a un thread a la fois et les timers locaux s'y attachent : les tests
/// (un thread chacun) passent l'un apres l'autre.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Fait tourner la boucle principale jusqu'a ce que `done` soit vrai, au plus `limit`.
fn run_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let ctx = glib::MainContext::default();
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        while ctx.iteration(false) {}
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

fn watch(dir: &Path) -> (DirWatch, Rc<RefCell<Vec<Vec<PathBuf>>>>) {
    let batches: Rc<RefCell<Vec<Vec<PathBuf>>>> = Rc::default();
    let w = DirWatch::new(dir, {
        let batches = batches.clone();
        move |paths| batches.borrow_mut().push(paths)
    })
    .expect("monitor");
    (w, batches)
}

#[test]
fn an_added_then_removed_file_is_reported_after_the_debounce() {
    let _serial = serial();
    let tmp = Tmp::new("add-remove");
    let (_w, batches) = watch(&tmp.0);
    let file = tmp.0.join("new.jpg");

    std::fs::write(&file, b"x").unwrap();
    let t0 = Instant::now();
    assert!(run_until(Duration::from_secs(5), || !batches.borrow().is_empty()), "ajout non signale");
    assert!(t0.elapsed() >= DEBOUNCE, "signale avant le debounce : {:?}", t0.elapsed());
    {
        let b = batches.borrow();
        assert_eq!(b.len(), 1, "un seul lot pour un ajout : {b:?}");
        assert_eq!(b[0], vec![file.clone()], "chemin dedoublonne");
    }

    batches.borrow_mut().clear();
    std::fs::remove_file(&file).unwrap();
    assert!(run_until(Duration::from_secs(5), || !batches.borrow().is_empty()), "suppression non signalee");
    assert_eq!(batches.borrow()[0], vec![file]);
}

#[test]
fn a_burst_of_events_makes_one_batch_and_hidden_files_are_left_out() {
    let _serial = serial();
    let tmp = Tmp::new("burst");
    let (_w, batches) = watch(&tmp.0);
    for n in 0..5 {
        std::fs::write(tmp.0.join(format!("p{n}.jpg")), b"x").unwrap();
    }
    std::fs::write(tmp.0.join(".hidden.jpg"), b"x").unwrap();
    assert!(run_until(Duration::from_secs(5), || !batches.borrow().is_empty()));
    // laisse passer d'eventuels evenements tardifs
    run_until(DEBOUNCE * 3, || false);
    let b = batches.borrow();
    assert_eq!(b.len(), 1, "{b:?}");
    let mut got: Vec<String> = b[0].iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    got.sort();
    assert_eq!(got, ["p0.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "p4.jpg"]);
}

#[test]
fn a_rename_reports_both_names_and_dropping_the_watch_stops_it() {
    let _serial = serial();
    let tmp = Tmp::new("rename");
    let a = tmp.0.join("a.jpg");
    std::fs::write(&a, b"x").unwrap();
    let (w, batches) = watch(&tmp.0);
    let b = tmp.0.join("b.jpg");
    std::fs::rename(&a, &b).unwrap();
    assert!(run_until(Duration::from_secs(5), || !batches.borrow().is_empty()));
    let mut got = batches.borrow().concat();
    got.sort();
    got.dedup();
    assert_eq!(got, vec![a, b.clone()]);

    drop(w);
    batches.borrow_mut().clear();
    std::fs::write(tmp.0.join("c.jpg"), b"x").unwrap();
    run_until(DEBOUNCE * 4, || false);
    assert!(batches.borrow().is_empty(), "surveillance arretee");
}
