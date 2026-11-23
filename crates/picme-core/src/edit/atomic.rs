//! Ecriture atomique : fichier temporaire cache `.picme-<pid>-<nom>.tmp` dans le dossier de la cible, fsync,
//! permissions et proprietaire de l'original, verification du temporaire relu sur disque, puis rename.
//!
//! Tant que [`Staged`] n'est pas valide, son drop supprime le temporaire : une ecriture qui echoue (erreur, panique,
//! verification refusee) ne laisse ni original altere ni fichier temporaire. Le mtime n'est pas conserve : un fichier
//! modifie doit changer de mtime (les miniatures et les caches s'y fient).

use crate::error::{Error, Result};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Au-dela, le nom de la cible est tronque dans celui du temporaire (NAME_MAX = 255 octets sur ext4, btrfs, xfs).
const MAX_NAME: usize = 200;

/// Temporaire ecrit et verifie, en attente du rename.
#[derive(Debug)]
pub struct Staged {
    tmp: PathBuf,
    done: bool,
}

/// Chemin du temporaire pour `target` : meme dossier (le rename reste sur le meme systeme de fichiers), nom cache
/// (ignore par le monitor et par la liste du dossier).
fn tmp_path(target: &Path) -> Result<PathBuf> {
    let name = target.file_name().ok_or_else(|| Error::Invalid(format!("pas de nom de fichier : {}", target.display())))?;
    let name = &name.as_bytes()[..name.len().min(MAX_NAME)];
    let mut tmp = format!(".picme-{}-", std::process::id()).into_bytes();
    tmp.extend_from_slice(name);
    tmp.extend_from_slice(b".tmp");
    Ok(target.with_file_name(OsStr::from_bytes(&tmp)))
}

/// Ecrit le contenu de `target` dans un temporaire a cote : `write` remplit le fichier, qui recoit ensuite les
/// permissions et le proprietaire de `perms_from` (s'il est donne) et un mtime plus recent que le sien, est
/// synchronise, puis relu par `check` (par son chemin) avant d'etre rendu. Rien n'est encore remplace : voir [`Staged::commit`] et [`Staged::commit_new`].
pub fn stage(
    target: &Path,
    perms_from: Option<&Path>,
    write: impl FnOnce(&mut File) -> Result<()>,
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<Staged> {
    let tmp = tmp_path(target)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    // a partir d'ici le drop du garde supprime le temporaire, y compris sur panique dans `write` ou `check`
    let staged = Staged { tmp, done: false };
    write(&mut file)?;
    file.flush()?;
    if let Some(src) = perms_from {
        let meta = fs::metadata(src)?;
        file.set_permissions(meta.permissions())?;
        // seul root peut donner le fichier a un autre ; le groupe passe si l'utilisateur en est membre
        let _ = std::os::unix::fs::fchown(&file, Some(meta.uid()), Some(meta.gid()));
        // l'horloge des mtime avance par pas de quelques ms : deux ecritures rapprochees auraient le meme mtime (et
        // souvent la meme taille), et les caches croiraient le fichier inchange. Le temporaire passe apres l'original.
        if let (Ok(old), Ok(new)) = (meta.modified(), file.metadata().and_then(|m| m.modified()))
            && new <= old
        {
            file.set_modified(old + Duration::from_nanos(1))?;
        }
    }
    file.sync_all()?;
    drop(file);
    check(&staged.tmp)?;
    Ok(staged)
}

impl Staged {
    /// Chemin du temporaire.
    pub fn path(&self) -> &Path {
        &self.tmp
    }

    /// Remplace `target` (meme dossier que le temporaire) par le temporaire, d'un seul rename. En cas d'echec le
    /// temporaire reste, pour un autre essai ou pour le drop.
    pub fn commit(&mut self, target: &Path) -> Result<()> {
        fs::rename(&self.tmp, target)?;
        self.done = true;
        sync_parent(target);
        Ok(())
    }

    /// Cree `target`, qui ne doit pas exister : lien dur puis suppression du temporaire, ce qui echoue en
    /// `AlreadyExists` sans rien ecraser si un autre programme a pris le nom entre-temps. Sur un systeme de fichiers
    /// sans liens durs, repli sur un rename apres verification (fenetre de course minime).
    pub fn commit_new(&mut self, target: &Path) -> Result<()> {
        match fs::hard_link(&self.tmp, target) {
            Ok(()) => {
                let _ = fs::remove_file(&self.tmp);
                self.done = true;
                sync_parent(target);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e.into()),
            Err(_) if fs::symlink_metadata(target).is_err() => {
                fs::rename(&self.tmp, target)?;
                self.done = true;
                sync_parent(target);
                Ok(())
            }
            Err(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists).into()),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.done {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

/// Synchronise le dossier pour que le rename survive a une coupure. Certains systemes de fichiers refusent le fsync
/// d'un dossier : sans consequence sur le resultat.
fn sync_parent(target: &Path) {
    if let Some(dir) = target.parent()
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
}

/// [`stage`] pour un contenu en memoire : le temporaire relu doit etre identique a `bytes` (ecriture complete),
/// puis `check` valide son contenu (dimensions, orientation, decodage).
pub fn stage_bytes(target: &Path, perms_from: Option<&Path>, bytes: &[u8], check: impl FnOnce(&[u8]) -> Result<()>) -> Result<Staged> {
    stage(target, perms_from, |f| Ok(f.write_all(bytes)?), |tmp| {
        let back = fs::read(tmp)?;
        if back != bytes {
            return Err(Error::Invalid(format!("relecture differente de l'ecrit : {}", tmp.display())));
        }
        check(&back)
    })
}

/// Remplace le fichier existant `path` par `bytes` en gardant ses permissions et son proprietaire. Un lien
/// symbolique est suivi : c'est le fichier pointe qui est reecrit, le lien reste.
pub fn replace(path: &Path, bytes: &[u8], check: impl FnOnce(&[u8]) -> Result<()>) -> Result<()> {
    let real = fs::canonicalize(path)?;
    stage_bytes(&real, Some(&real), bytes, check)?.commit(&real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use std::os::unix::fs::PermissionsExt;

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(".picme-")).collect();
        v.sort();
        v
    }

    #[test]
    fn replace_keeps_permissions_and_changes_content() {
        let tmp = TmpDir::new("atomic-ok");
        let p = tmp.path().join("x.bin");
        fs::write(&p, b"avant").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        let ino = fs::metadata(&p).unwrap().ino();
        let mut seen = None;
        replace(&p, b"apres, plus long", |back| {
            seen = Some(back.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.as_deref(), Some(&b"apres, plus long"[..]), "check relit le temporaire ecrit");
        assert_eq!(fs::read(&p).unwrap(), b"apres, plus long");
        let meta = fs::metadata(&p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_ne!(meta.ino(), ino, "rename : nouveau fichier, pas une reecriture en place");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn the_new_mtime_is_always_after_the_old_one() {
        let tmp = TmpDir::new("atomic-mtime");
        let p = tmp.path().join("x.bin");
        fs::write(&p, b"avant").unwrap();
        // original date dans le futur : le cas extreme de deux ecritures dans le meme pas d'horloge
        let future = std::time::SystemTime::now() + Duration::from_secs(60);
        File::options().write(true).open(&p).unwrap().set_modified(future).unwrap();
        replace(&p, b"apres", |_| Ok(())).unwrap();
        assert!(fs::metadata(&p).unwrap().modified().unwrap() > future);
    }

    #[test]
    fn write_failure_midway_leaves_original_and_no_temp() {
        let tmp = TmpDir::new("atomic-write-fail");
        let p = tmp.path().join("x.bin");
        fs::write(&p, b"original").unwrap();
        let r = stage(
            &p,
            Some(&p),
            |f| {
                f.write_all(b"moitie d'un fich")?;
                Err(Error::Io(io::Error::other("disque plein simule")))
            },
            |_| panic!("check ne doit pas etre appele apres un echec d'ecriture"),
        );
        assert!(r.is_err());
        assert_eq!(fs::read(&p).unwrap(), b"original");
        assert!(leftovers(tmp.path()).is_empty(), "temporaire restant : {:?}", leftovers(tmp.path()));
    }

    #[test]
    fn failed_check_leaves_original_and_no_temp() {
        let tmp = TmpDir::new("atomic-check-fail");
        let p = tmp.path().join("x.bin");
        fs::write(&p, b"original").unwrap();
        let r = replace(&p, b"resultat invalide", |_| Err(Error::Invalid("dimensions inattendues".into())));
        assert!(matches!(r, Err(Error::Invalid(_))));
        assert_eq!(fs::read(&p).unwrap(), b"original");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn panic_while_writing_leaves_no_temp() {
        let tmp = TmpDir::new("atomic-panic");
        let p = tmp.path().join("x.bin");
        fs::write(&p, b"original").unwrap();
        let r = std::panic::catch_unwind(|| {
            let _ = stage(
                &p,
                None,
                |f| {
                    f.write_all(b"debut").unwrap();
                    panic!("panique simulee en cours d'ecriture")
                },
                |_| Ok(()),
            );
        });
        assert!(r.is_err());
        assert_eq!(fs::read(&p).unwrap(), b"original");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn staged_dropped_without_commit_is_removed() {
        let tmp = TmpDir::new("atomic-drop");
        let p = tmp.path().join("x.bin");
        let s = stage(&p, None, |f| Ok(f.write_all(b"x")?), |_| Ok(())).unwrap();
        assert!(s.path().exists());
        drop(s);
        assert!(!p.exists());
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn commit_new_never_overwrites() {
        let tmp = TmpDir::new("atomic-new");
        let p = tmp.path().join("x-crop.bin");
        stage(&p, None, |f| Ok(f.write_all(b"un")?), |_| Ok(())).unwrap().commit_new(&p).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"un");
        let r = stage(&p, None, |f| Ok(f.write_all(b"deux")?), |_| Ok(())).unwrap().commit_new(&p);
        assert!(matches!(r, Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(fs::read(&p).unwrap(), b"un");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn symlink_target_is_rewritten_and_link_kept() {
        let tmp = TmpDir::new("atomic-link");
        let real = tmp.path().join("real.bin");
        let link = tmp.path().join("link.bin");
        fs::write(&real, b"avant").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        replace(&link, b"apres", |_| Ok(())).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"apres");
    }

    #[test]
    fn long_names_fit_in_name_max() {
        let name = format!("{}.jpg", "n".repeat(250));
        let t = tmp_path(&Path::new("/x").join(&name)).unwrap();
        assert!(t.file_name().unwrap().len() <= 255);
    }
}
