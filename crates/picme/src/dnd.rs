//! Glisser-deposer. Depuis le viewer : l'image courante (et le `.mov` d'une Live Photo) part en `GdkFileList`, que GDK
//! serialise en `text/uri-list` pour les autres programmes ; copie ou deplacement selon l'action que la cible negocie.
//! Vers le viewer : un fichier depose s'ouvre, son dossier devient le courant. Sur une image au fit le glisser
//! emporte le fichier ; au-dessus du fit il deplace l'image (pan) et le fichier part avec Maj+glisser.

use crate::canvas::Canvas;
use gtk4 as gtk;
use gtk::{gdk, gio, glib, prelude::*};
use picme_core::dir::Entry;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Les fichiers que le glisser emporte : l'image, puis le `.mov` de sa Live Photo (qui suit l'image, comme a la
/// corbeille).
pub fn files_of(e: &Entry) -> Vec<PathBuf> {
    std::iter::once(e.path.clone()).chain(e.live_mov.clone()).collect()
}

/// Contenu du glisser : une `GdkFileList`.
pub fn content(paths: &[PathBuf]) -> gdk::ContentProvider {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value())
}

/// Le fichier a ouvrir parmi ceux deposes : le premier qui a un chemin local et qui existe.
pub fn dropped_path(files: &[gio::File]) -> Option<PathBuf> {
    files.iter().filter_map(|f| f.path()).find(|p: &PathBuf| Path::new(p).exists())
}

/// Le glisser depuis `canvas`. `files` rend les fichiers du courant au moment du glisser ; `moved` recoit ceux que la
/// cible a deplaces (action Move reussie), pour les retirer de la vue.
pub fn attach_source(canvas: &Canvas, files: impl Fn() -> Vec<PathBuf> + 'static, moved: impl Fn(Vec<PathBuf>) + 'static) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let carried: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    source.connect_prepare({
        let canvas = canvas.downgrade();
        let carried = carried.clone();
        move |source, _, _| {
            let canvas = canvas.upgrade()?;
            // au-dessus du fit le glisser est le pan, sauf avec Maj
            let shift = source.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            if canvas.is_zoomed() && !shift {
                return None;
            }
            let paths = files();
            if paths.is_empty() {
                return None;
            }
            let provider = content(&paths);
            *carried.borrow_mut() = paths;
            Some(provider)
        }
    });
    // le nom du fichier suit le curseur : une miniature n'est pas disponible a la taille voulue sans decode
    source.connect_drag_begin({
        let carried = carried.clone();
        move |_, drag| {
            let name = carried.borrow().first().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
            let label = gtk::Label::new(Some(&name));
            label.add_css_class("osd");
            gtk::DragIcon::for_drag(drag).downcast::<gtk::DragIcon>().expect("DragIcon").set_child(Some(&label));
        }
    });
    source.connect_drag_end(move |_, _, delete_data| {
        let paths = carried.take();
        if delete_data {
            moved(paths);
        }
    });
    canvas.add_controller(source);
}

/// La reception sur `widget` (la fenetre) : `open` recoit le fichier depose. Le fichier n'est qu'ouvert : GTK retient la
/// copie quand elle est offerte, donc la source ne supprime rien (Maj force le deplacement chez la source ; le
/// deplacement n'est accepte que pour que ce geste ne soit pas refuse). Un glisser parti de cette fenetre est refuse.
pub fn attach_target(widget: &impl IsA<gtk::Widget>, open: impl Fn(PathBuf) + 'static) {
    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY | gdk::DragAction::MOVE);
    target.set_types(&[gdk::FileList::static_type(), gio::File::static_type()]);
    target.connect_accept(|_, drop| drop.drag().is_none());
    target.connect_drop(move |_, value, _, _| {
        let files: Vec<gio::File> = match value.get::<gdk::FileList>() {
            Ok(list) => list.files(),
            Err(_) => match value.get::<gio::File>() {
                Ok(f) => vec![f],
                Err(_) => return false,
            },
        };
        match dropped_path(&files) {
            Some(path) => {
                open(path);
                true
            }
            None => false,
        }
    });
    widget.add_controller(target);
}

/// Les URI d'un contenu de glisser, tels qu'un autre programme les lirait (`text/uri-list`) : GDK serialise la
/// `GdkFileList` du fournisseur, comme il le fait pour la cible d'un glisser.
pub fn uri_list(provider: &gdk::ContentProvider) -> Result<String, glib::Error> {
    let value = provider.value(gdk::FileList::static_type())?;
    let stream = gio::MemoryOutputStream::new_resizable();
    glib::MainContext::default().block_on(gdk::content_serialize_future(&stream, "text/uri-list", &value, glib::Priority::DEFAULT))?;
    stream.close(gio::Cancellable::NONE)?;
    Ok(String::from_utf8_lossy(&stream.steal_as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use picme_core::media::MediaKind;

    fn entry(path: &str, live: Option<&str>) -> Entry {
        Entry { path: PathBuf::from(path), kind: MediaKind::Jpeg, live_mov: live.map(PathBuf::from), mtime_ns: 0, size: 0 }
    }

    #[test]
    fn a_live_photo_carries_its_mov() {
        assert_eq!(files_of(&entry("/p/a.heic", None)), [PathBuf::from("/p/a.heic")]);
        assert_eq!(files_of(&entry("/p/a.heic", Some("/p/a.mov"))), [PathBuf::from("/p/a.heic"), PathBuf::from("/p/a.mov")]);
    }

    // la serialisation de GDK interroge l'affichage : ce test n'est lance que sur un Xvfb, jamais sur l'ecran de
    // l'utilisateur (DISPLAY=:99 cargo test -p picme --lib -- --ignored)
    #[test]
    #[ignore = "a besoin d'un affichage : Xvfb"]
    fn the_drag_content_is_a_uri_list_of_the_files() {
        gtk4::init().expect("affichage");
        let tmp = TmpDir::new("dnd-content");
        let (a, b) = (tmp.path().join("a b.jpg"), tmp.path().join("a b.mov"));
        let provider = content(&[a.clone(), b.clone()]);
        // le contenu est lisible comme liste de fichiers par un programme de ce processus, et comme `text/uri-list` par les autres
        let list = provider.value(gdk::FileList::static_type()).unwrap().get::<gdk::FileList>().unwrap();
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
        assert_eq!(paths, [a.clone(), b.clone()]);
        let mimes = provider.formats().union_serialize_mime_types();
        assert!(mimes.contain_mime_type("text/uri-list"), "{}", mimes.to_str());
        let uris = uri_list(&provider).unwrap();
        let want = format!("{}\r\n{}\r\n", gio::File::for_path(&a).uri(), gio::File::for_path(&b).uri());
        assert_eq!(uris, want);
        assert!(uris.contains("a%20b.jpg"), "espaces codes : {uris}");
    }

    #[test]
    fn a_drop_opens_the_first_local_file_that_exists() {
        let tmp = TmpDir::new("dnd-drop");
        let real = tmp.path().join("x.jpg");
        std::fs::write(&real, b"x").unwrap();
        let files = [gio::File::for_path(tmp.path().join("absent.jpg")), gio::File::for_uri("http://example.org/y.jpg"), gio::File::for_path(&real)];
        assert_eq!(dropped_path(&files), Some(real));
        assert_eq!(dropped_path(&files[..2]), None);
        assert_eq!(dropped_path(&[]), None);
    }
}
