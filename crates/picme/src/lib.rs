//! Partie GTK de picme, en bibliotheque pour que le controller et ses tests vivent hors de `main.rs`.

pub mod app;
pub mod canvas;
pub mod chrome;
pub mod crop;
pub mod dnd;
pub mod fileops;
pub mod filmstrip;
pub mod help;
pub mod keys;
pub mod launch;
pub mod monitor;
pub mod osd;
pub mod trashbin;
pub mod video;
pub mod view;

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    /// Dossier de test jetable sous `target/test-tmp` (pas de /tmp, qui est en RAM), supprime au drop.
    pub struct TmpDir(PathBuf);

    impl TmpDir {
        pub fn new(name: &str) -> Self {
            let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp").join(format!("{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            // chemins canoniques : le controller compare des chemins absolus
            TmpDir(p.canonicalize().unwrap())
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
