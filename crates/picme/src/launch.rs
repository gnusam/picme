//! Ligne de commande : ce que `picme` ouvre selon son argument. Sans GTK, pour se tester seul ; `main` l'appelle avant
//! les pools et `gtk::init`, de sorte qu'une erreur ne laisse aucune fenetre.

use picme_core::dir;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "\
picme : visionneuse d'images et de vidéos

Usage : picme [FICHIER|DOSSIER]...

  (rien)   ouvre le dossier courant
  DOSSIER  ouvre ce dossier et affiche son premier média, dans l'ordre de navigation
  FICHIER  affiche ce fichier ; son dossier est la liste de navigation
           (plusieurs fichiers : seul le premier compte)

Options :
  -h, --help     affiche cette aide
  -V, --version  affiche la version
";

pub fn version() -> String {
    format!("picme {}", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    Help,
    Version,
    /// Le premier chemin donne, s'il y en a un.
    Open(Option<PathBuf>),
}

/// Les arguments apres le nom du programme. `--` finit les options (un fichier nomme `-x`).
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Cli, String> {
    let mut path = None;
    let mut options = true;
    for arg in args {
        if options {
            match arg.to_str() {
                Some("-h" | "--help") => return Ok(Cli::Help),
                Some("-V" | "--version") => return Ok(Cli::Version),
                Some("--") => {
                    options = false;
                    continue;
                }
                Some(s) if s.len() > 1 && s.starts_with('-') => return Err(format!("option inconnue : {s}")),
                _ => {}
            }
        }
        path.get_or_insert_with(|| PathBuf::from(arg));
    }
    Ok(Cli::Open(path))
}

/// Ce que l'application ouvre.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    /// Le dossier de navigation, absolu et canonique.
    pub dir: PathBuf,
    /// Le fichier a afficher en premier ; `None` : le premier media du dossier.
    pub file: Option<PathBuf>,
    /// Ce que l'OSD doit dire (fichier donne mais pas pris en charge).
    pub notice: Option<String>,
}

/// L'erreur d'E/S dite pour stderr.
pub fn reason(e: &io::Error) -> String {
    dir::io_reason(e)
}

/// `arg` (rien : le dossier courant `cwd`) -> dossier et fichier initial. Un chemin relatif se lit depuis `cwd`. Un
/// dossier devient canonique (liens symboliques suivis) ; pour un fichier, le dossier l'est et le nom reste tel quel
/// (un lien symbolique se parcourt dans le dossier ou il est, comme `Controller::open`). Un fichier existant mais pas
/// pris en charge (contenu sans signature de media, meme avec une extension d'image) n'est pas une erreur : son
/// dossier s'ouvre, avec un `notice` qui dit pourquoi. Un chemin absent ou illisible est l'erreur, deja formulee pour
/// stderr.
pub fn resolve(arg: Option<&Path>, cwd: &Path) -> Result<Launch, String> {
    let shown = arg.unwrap_or(cwd).display();
    let path = arg.map_or_else(|| cwd.to_path_buf(), |a| cwd.join(a));
    let md = std::fs::metadata(&path).map_err(|e| format!("{shown} : {}", reason(&e)))?;
    if md.is_dir() {
        let dir = std::fs::canonicalize(&path).map_err(|e| format!("{shown} : {}", reason(&e)))?;
        return Ok(Launch { dir, file: None, notice: None });
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("{shown} : ni dossier ni fichier"));
    };
    let dir = std::fs::canonicalize(parent).map_err(|e| format!("{shown} : {}", reason(&e)))?;
    let file = dir.join(name);
    // le fichier est nomme : son contenu tranche, quelle que soit son extension
    if dir::stat_entry_explicit(&file).is_some() {
        return Ok(Launch { dir, file: Some(file), notice: None });
    }
    Ok(Launch { dir, file: None, notice: Some(format!("{} : {}", name.to_string_lossy(), dir::reject_reason(&file))) })
}

/// Message de l'OSD d'un dossier sans media.
pub fn empty_message(dir: &Path) -> String {
    format!("Aucun média dans {}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use std::os::unix::fs::symlink;

    fn args(v: &[&str]) -> Result<Cli, String> {
        parse_args(v.iter().map(OsString::from))
    }

    #[test]
    fn parses_help_version_and_the_first_path() {
        assert_eq!(args(&[]), Ok(Cli::Open(None)));
        assert_eq!(args(&["a.jpg", "b.jpg"]), Ok(Cli::Open(Some("a.jpg".into()))));
        assert_eq!(args(&["--help"]), Ok(Cli::Help));
        assert_eq!(args(&["-h"]), Ok(Cli::Help));
        assert_eq!(args(&["a.jpg", "--version"]), Ok(Cli::Version));
        assert_eq!(args(&["-V"]), Ok(Cli::Version));
        assert_eq!(args(&["--zoom"]), Err("option inconnue : --zoom".to_string()));
        // `--` : la suite est un chemin, meme avec un tiret
        assert_eq!(args(&["--", "-x.jpg", "--help"]), Ok(Cli::Open(Some("-x.jpg".into()))));
        assert_eq!(args(&["-"]), Ok(Cli::Open(Some("-".into()))));
    }

    #[test]
    fn no_argument_opens_the_current_directory() {
        let tmp = TmpDir::new("launch-cwd");
        assert_eq!(resolve(None, tmp.path()), Ok(Launch { dir: tmp.path().to_path_buf(), file: None, notice: None }));
    }

    #[test]
    fn a_directory_is_opened_without_initial_file_whatever_its_spelling() {
        let tmp = TmpDir::new("launch-dir");
        let sub = tmp.path().join("mes photos");
        std::fs::create_dir(&sub).unwrap();
        symlink(&sub, tmp.path().join("lien")).unwrap();
        let want = Ok(Launch { dir: sub.clone(), file: None, notice: None });
        // relatif, avec espaces, avec `..` et `.`, avec barre finale, par lien symbolique, absolu
        for arg in ["mes photos", "./mes photos/", "mes photos/../mes photos/.", "lien", "lien/"] {
            assert_eq!(resolve(Some(Path::new(arg)), tmp.path()), want, "{arg}");
        }
        assert_eq!(resolve(Some(&sub), Path::new("/")), want);
        assert_eq!(resolve(Some(Path::new(".")), &sub), want);
        assert_eq!(resolve(Some(Path::new("..")), &sub), Ok(Launch { dir: tmp.path().to_path_buf(), file: None, notice: None }));
    }

    #[test]
    fn a_file_is_shown_and_its_directory_is_the_list() {
        let tmp = TmpDir::new("launch-file");
        let sub = tmp.path().join("a b");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("x y.JPG"), b"x").unwrap();
        std::fs::write(sub.join("v.mov"), b"x").unwrap();
        symlink(&sub, tmp.path().join("lien")).unwrap();
        symlink(sub.join("x y.JPG"), tmp.path().join("raccourci.jpg")).unwrap();
        let want = Ok(Launch { dir: sub.clone(), file: Some(sub.join("x y.JPG")), notice: None });
        assert_eq!(resolve(Some(Path::new("a b/x y.JPG")), tmp.path()), want);
        assert_eq!(resolve(Some(Path::new("x y.JPG")), &sub), want);
        assert_eq!(resolve(Some(Path::new("./../a b/./x y.JPG")), &sub), want);
        assert_eq!(resolve(Some(&sub.join("x y.JPG")), Path::new("/")), want);
        // dossier parent par lien symbolique : canonique ; le fichier garde son nom
        assert_eq!(resolve(Some(Path::new("lien/x y.JPG")), tmp.path()), want);
        // lien symbolique vers un fichier : il reste dans le dossier ou il est
        assert_eq!(
            resolve(Some(Path::new("raccourci.jpg")), tmp.path()),
            Ok(Launch { dir: tmp.path().to_path_buf(), file: Some(tmp.path().join("raccourci.jpg")), notice: None })
        );
        // une video est un media
        assert_eq!(resolve(Some(Path::new("v.mov")), &sub).unwrap().file, Some(sub.join("v.mov")));
    }

    #[test]
    fn an_unsupported_file_opens_its_directory_with_a_notice() {
        let tmp = TmpDir::new("launch-unsupported");
        std::fs::write(tmp.path().join("notes de voyage.txt"), b"x").unwrap();
        assert_eq!(
            resolve(Some(Path::new("notes de voyage.txt")), tmp.path()),
            Ok(Launch {
                dir: tmp.path().to_path_buf(),
                file: None,
                notice: Some("notes de voyage.txt : contenu non reconnu comme média".to_string())
            })
        );
    }

    #[test]
    fn a_file_is_opened_by_its_content_whatever_its_extension() {
        let tmp = TmpDir::new("launch-content");
        let jpeg = std::fs::read(picme_core::fixtures_dir().join("a.jpg")).unwrap();
        for name in ["T984.ithmb", "photo.txt", "sans-extension"] {
            std::fs::write(tmp.path().join(name), &jpeg).unwrap();
            let got = resolve(Some(Path::new(name)), tmp.path()).unwrap();
            assert_eq!(got.file, Some(tmp.path().join(name)), "{name}");
            assert_eq!(got.notice, None, "{name}");
        }
        // un dossier qui porte une extension d'image n'est pas un fichier
        std::fs::create_dir(tmp.path().join("d.ithmb")).unwrap();
        assert_eq!(resolve(Some(Path::new("d.ithmb")), tmp.path()).unwrap().file, None);
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let tmp = TmpDir::new("launch-missing");
        symlink(tmp.path().join("nulle part"), tmp.path().join("casse.jpg")).unwrap();
        for arg in ["absent.jpg", "absent/", "absent/x.jpg", "casse.jpg"] {
            let err = resolve(Some(Path::new(arg)), tmp.path()).unwrap_err();
            assert_eq!(err, format!("{arg} : introuvable"), "{arg}");
        }
    }

    #[test]
    fn the_empty_message_names_the_directory() {
        assert_eq!(empty_message(Path::new("/home/sam/vide")), "Aucun média dans /home/sam/vide");
    }
}
