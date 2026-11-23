//! Corbeille et suppression reelles par GIO, dans un HOME et un XDG_DATA_HOME temporaires : jamais la corbeille de
//! l'utilisateur. Binaire de test a part, et un seul test : glib lit ces variables une fois pour toutes, au premier
//! appel, et elles doivent etre posees avant tout autre thread.

use gtk4::glib;
use picme::fileops::{Done, FileOps};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn removed(rx: &async_channel::Receiver<Done>) -> (Vec<std::path::PathBuf>, bool, Option<String>, String) {
    let done = rx.recv_blocking().expect("compte rendu");
    let msg = done.message();
    match done {
        Done::Removed { paths, trash, error } => (paths, trash, error.map(|(p, e)| format!("{} : {e}", p.display())), msg),
        other => panic!("attendu Removed, recu {other:?}"),
    }
}

#[test]
fn trash_and_delete_go_through_gio_into_a_private_trash() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("picme-trash-{}", std::process::id()));
    let (home, data, photos) = (root.join("home"), root.join("home/.local/share"), root.join("photos"));
    let _ = std::fs::remove_dir_all(&root);
    for d in [&data, &photos] {
        std::fs::create_dir_all(d).unwrap();
    }
    // SAFETY : premier code du seul test de ce binaire, avant tout appel a glib et tout autre thread
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", &data);
    }
    // on s'arrete si glib regarde ailleurs, ou si la corbeille irait a la racine d'un autre systeme de fichiers
    assert_eq!(glib::home_dir(), home);
    assert_eq!(glib::user_data_dir(), data);
    assert_eq!(std::fs::metadata(&photos).unwrap().dev(), std::fs::metadata(&home).unwrap().dev());

    let (tx, rx) = async_channel::unbounded();
    let ops = FileOps::new(None, tx);
    let h = ops.handle();

    // Suppr sur une Live Photo : l'image puis son .mov, chacun avec son .trashinfo
    let (img, mov) = (photos.join("IMG_1.HEIC"), photos.join("IMG_1.MOV"));
    std::fs::write(&img, b"image").unwrap();
    std::fs::write(&mov, b"video").unwrap();
    assert!(h.trash(vec![img.clone(), mov.clone()]));
    let (paths, trash, error, msg) = removed(&rx);
    assert_eq!((paths, trash, error), (vec![img.clone(), mov.clone()], true, None));
    assert_eq!(msg, "IMG_1.HEIC et son .mov à la corbeille");
    let bin = data.join("Trash");
    for (p, content) in [(&img, "image"), (&mov, "video")] {
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(!p.exists(), "{name} encore dans le dossier");
        assert_eq!(std::fs::read_to_string(bin.join("files").join(name)).unwrap(), content);
        let info = std::fs::read_to_string(bin.join("info").join(format!("{name}.trashinfo"))).unwrap();
        assert!(info.starts_with("[Trash Info]\n"), "{info}");
        assert!(info.lines().any(|l| l == format!("Path={}", p.display())), "{info}");
        assert!(info.lines().any(|l| l.starts_with("DeletionDate=")), "{info}");
    }

    // Maj+Suppr : definitif, rien dans la corbeille
    let jpg = photos.join("b.jpg");
    std::fs::write(&jpg, b"x").unwrap();
    assert!(h.delete(vec![jpg.clone()]));
    let (_, trash, error, msg) = removed(&rx);
    assert_eq!((trash, error, msg.as_str()), (false, None, "b.jpg supprimé"));
    assert!(!jpg.exists() && !bin.join("files/b.jpg").exists());

    // fichier deja parti : l'erreur remonte, rien d'autre n'est tente
    let gone = photos.join("parti.jpg");
    let after = photos.join("apres.jpg");
    std::fs::write(&after, b"x").unwrap();
    assert!(h.trash(vec![gone.clone(), after.clone()]));
    let (_, _, error, msg) = removed(&rx);
    assert!(error.is_some_and(|e| e.starts_with(gone.to_str().unwrap())));
    assert!(msg.starts_with("corbeille impossible : parti.jpg"), "{msg}");
    assert!(after.exists() && !bin.join("files/apres.jpg").exists());

    ops.finish();
    // aucune corbeille de secours a cote des photos
    let stray: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().starts_with(".Trash")).collect();
    assert!(stray.is_empty(), "{stray:?}");
    std::fs::remove_dir_all(&root).unwrap();
}
