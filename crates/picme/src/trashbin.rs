//! Retrouver, restaurer ou vider ce que GIO vient de mettre a la corbeille freedesktop. GIO ne dit pas ou le fichier
//! est alle : on relit les `.trashinfo` (`Path=`, `DeletionDate=`) de la corbeille personnelle et des corbeilles des
//! points de montage (`<racine>/.Trash-<uid>`, `<racine>/.Trash/<uid>`, chemins relatifs a la racine).

use gtk4::glib;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Un fichier a la corbeille : son emplacement d'origine, son contenu et sa fiche `.trashinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    pub orig: PathBuf,
    pub file: PathBuf,
    pub info: PathBuf,
}

/// Les dossiers de corbeille ou `orig` a pu aller, avec la racine a laquelle ses `Path=` relatifs se rapportent.
fn bins(orig: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut bins = vec![(glib::user_data_dir().join("Trash"), None)];
    let uid = std::fs::metadata(glib::user_data_dir()).map(|m| m.uid()).unwrap_or(0);
    for top in orig.ancestors().skip(1) {
        bins.push((top.join(format!(".Trash-{uid}")), Some(top.to_path_buf())));
        bins.push((top.join(".Trash").join(uid.to_string()), Some(top.to_path_buf())));
    }
    bins
}

/// Le `.trashinfo` le plus recent de `orig` dans une corbeille, avec le `Trashed` qui va avec.
fn find_in(bin: &Path, top: Option<&Path>, orig: &Path) -> Option<(String, Trashed)> {
    let mut best: Option<(String, Trashed)> = None;
    for entry in std::fs::read_dir(bin.join("info")).ok()?.flatten() {
        let info = entry.path();
        if info.extension().is_none_or(|e| e != "trashinfo") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&info) else { continue };
        let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)).map(str::trim);
        let (Some(path), Some(date)) = (field("Path="), field("DeletionDate=")) else { continue };
        let Some(path) = glib::uri_unescape_string(path, None::<&str>) else { continue };
        let full = match top {
            Some(top) if Path::new(path.as_str()).is_relative() => top.join(path.as_str()),
            _ => PathBuf::from(path.as_str()),
        };
        if full != orig {
            continue;
        }
        let Some(stem) = info.file_stem() else { continue };
        let file = bin.join("files").join(stem);
        if file.symlink_metadata().is_err() || best.as_ref().is_some_and(|(d, _)| d.as_str() >= date) {
            continue;
        }
        best = Some((date.to_string(), Trashed { orig: orig.to_path_buf(), file, info }));
    }
    best
}

/// Ou est `orig`, a peine mis a la corbeille : la fiche la plus recente de toutes les corbeilles possibles.
pub fn locate(orig: &Path) -> Option<Trashed> {
    bins(orig).into_iter().filter_map(|(bin, top)| find_in(&bin, top.as_deref(), orig)).max_by(|a, b| a.0.cmp(&b.0)).map(|(_, t)| t)
}

/// Remet le fichier a sa place. Refuse d'ecraser : un autre fichier a pris le nom depuis.
pub fn restore(t: &Trashed) -> std::io::Result<()> {
    if t.orig.symlink_metadata().is_ok() {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "un fichier de ce nom existe deja"));
    }
    if let Some(parent) = t.orig.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&t.file, &t.orig)?;
    let _ = std::fs::remove_file(&t.info);
    Ok(())
}

/// Vide l'element de la corbeille : le contenu puis sa fiche.
pub fn purge(t: &Trashed) -> std::io::Result<()> {
    if t.file.is_dir() && !t.file.is_symlink() {
        std::fs::remove_dir_all(&t.file)?;
    } else {
        std::fs::remove_file(&t.file)?;
    }
    let _ = std::fs::remove_file(&t.info);
    Ok(())
}
