//! Coeur de picme, sans dependance GTK : decode, orientation, thumbnails, cache, edition de fichiers.

use std::path::{Path, PathBuf};

/// Dossier des fixtures de test (`tests/fixtures` a la racine du workspace).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures attendues apres `tests/fixtures/make.sh`.
    const FIXTURES: &[&str] = &[
        "a.jpg", "b_333x217.jpg", "c_orient6.jpg", "d.png", "e.tiff", "f.webp", "g.heic", "h.avif", "i.jxl", "j.mov",
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
