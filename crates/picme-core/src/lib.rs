//! Coeur de picme, sans dependance GTK : decode, orientation, thumbnails, cache, edition de fichiers.

pub mod decode;
pub mod dir;
pub mod error;
pub mod media;
pub mod orient;

pub use error::{Error, Result};

use std::path::{Path, PathBuf};

/// Dossier des fixtures de test (`tests/fixtures` a la racine du workspace).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

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
            TmpDir(p)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures attendues apres `tests/fixtures/make.sh`.
    const FIXTURES: &[&str] = &[
        "a.jpg", "b_333x217.jpg", "c_orient6.jpg", "d.png", "e.tiff", "f.webp", "g.heic", "h.avif", "i.jxl", "j.mov", "k_thumb.heic",
    ];

    #[test]
    fn fixtures_present_and_non_empty() {
        for name in FIXTURES {
            let p = fixtures_dir().join(name);
            let len = std::fs::metadata(&p).unwrap_or_else(|e| panic!("{}: {e} (lancer tests/fixtures/make.sh)", p.display())).len();
            assert!(len > 0, "{} est vide", p.display());
        }
    }
}
