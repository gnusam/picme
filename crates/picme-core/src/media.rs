//! Detection du type de media : magic bytes d'abord, extension en repli.

use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Jpeg,
    Png,
    Webp,
    Tiff,
    /// HEIC et AVIF : un seul chemin (libheif).
    Heif,
    Jxl,
    Raw,
    Video,
}

/// Octets lus en tete de fichier : assez pour la box `ftyp` et ses premieres brands.
const HEAD_LEN: usize = 64;

/// Brands `ftyp` des images HEIF (HEIC, AVIF et leurs variantes).
const HEIF_BRANDS: &[&[u8; 4]] = &[
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"mif2", b"msf1", b"avif", b"avis",
];

/// Type d'apres les premiers octets seuls. `None` si aucune signature connue (le repli est l'extension).
pub fn sniff(head: &[u8]) -> Option<MediaKind> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(MediaKind::Jpeg);
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(MediaKind::Png);
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" {
        return match &head[8..12] {
            b"WEBP" => Some(MediaKind::Webp),
            b"AVI " => Some(MediaKind::Video),
            _ => None,
        };
    }
    // TIFF (II*\0, MM\0*) ; ORF (IIRO, IIRS) et RW2 (IIU\0) sont des TIFF a magic modifie, donc RAW.
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        return Some(MediaKind::Tiff);
    }
    if head.starts_with(b"IIRO") || head.starts_with(b"IIRS") || head.starts_with(b"IIU\0") {
        return Some(MediaKind::Raw);
    }
    if head.starts_with(b"FUJIFILMCCD-RAW") || (head.len() >= 14 && &head[6..14] == b"HEAPCCDR") {
        return Some(MediaKind::Raw);
    }
    // JPEG XL : codestream nu, ou container ISOBMFF "JXL ".
    if head.starts_with(&[0xFF, 0x0A]) || head.starts_with(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A]) {
        return Some(MediaKind::Jxl);
    }
    // Matroska et WebM.
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(MediaKind::Video);
    }
    // ISOBMFF : HEIC, AVIF, MOV et MP4 commencent tous par une box ftyp, seule la brand les distingue.
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let major: &[u8; 4] = head[8..12].try_into().unwrap();
        if major == b"crx " {
            return Some(MediaKind::Raw);
        }
        if HEIF_BRANDS.contains(&major) {
            return Some(MediaKind::Heif);
        }
        return Some(MediaKind::Video);
    }
    None
}

/// Type d'apres l'extension (insensible a la casse). `None` pour une extension inconnue.
pub fn kind_from_extension(ext: &str) -> Option<MediaKind> {
    let kind = match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => MediaKind::Jpeg,
        "png" => MediaKind::Png,
        "webp" => MediaKind::Webp,
        "tif" | "tiff" => MediaKind::Tiff,
        "heic" | "heif" | "hif" | "avif" => MediaKind::Heif,
        "jxl" => MediaKind::Jxl,
        "dng" | "cr2" | "cr3" | "nef" | "nrw" | "arw" | "orf" | "rw2" | "raf" | "pef" | "srw" => MediaKind::Raw,
        "mov" | "mp4" | "m4v" | "mkv" | "webm" | "avi" | "3gp" => MediaKind::Video,
        _ => return None,
    };
    Some(kind)
}

/// Type d'apres l'extension du chemin, sans ouvrir le fichier.
pub fn kind_from_path(path: &Path) -> Option<MediaKind> {
    kind_from_extension(path.extension()?.to_str()?)
}

/// Type du fichier : magic bytes, repli sur l'extension si le fichier est illisible ou sans signature connue.
/// Les RAW a base TIFF (DNG, NEF, ARW...) ont le magic TIFF : l'extension tranche pour eux.
pub fn detect(path: &Path) -> Option<MediaKind> {
    let by_ext = kind_from_path(path);
    let mut head = [0u8; HEAD_LEN];
    let n = File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    match (sniff(&head[..n]), by_ext) {
        (Some(MediaKind::Tiff), Some(MediaKind::Raw)) => Some(MediaKind::Raw),
        (Some(k), _) => Some(k),
        (None, ext) => ext,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures_dir;

    #[test]
    fn detects_every_fixture_by_magic() {
        let expected = [
            ("a.jpg", MediaKind::Jpeg),
            ("b_333x217.jpg", MediaKind::Jpeg),
            ("c_orient6.jpg", MediaKind::Jpeg),
            ("d.png", MediaKind::Png),
            ("e.tiff", MediaKind::Tiff),
            ("f.webp", MediaKind::Webp),
            ("g.heic", MediaKind::Heif),
            ("h.avif", MediaKind::Heif),
            ("i.jxl", MediaKind::Jxl),
            ("j.mov", MediaKind::Video),
        ];
        for (name, kind) in expected {
            let p = fixtures_dir().join(name);
            let mut head = [0u8; HEAD_LEN];
            let n = File::open(&p).unwrap().read(&mut head).unwrap();
            assert_eq!(sniff(&head[..n]), Some(kind), "magic de {name}");
            assert_eq!(detect(&p), Some(kind), "detect de {name}");
        }
    }

    #[test]
    fn magic_wins_over_a_wrong_extension() {
        let dir = crate::testutil::TmpDir::new("media-wrong-ext");
        let p = dir.path().join("photo.png");
        std::fs::copy(fixtures_dir().join("a.jpg"), &p).unwrap();
        assert_eq!(detect(&p), Some(MediaKind::Jpeg));
    }

    #[test]
    fn extension_is_the_fallback() {
        let dir = crate::testutil::TmpDir::new("media-fallback");
        let empty = dir.path().join("vide.webp");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(detect(&empty), Some(MediaKind::Webp));
        assert_eq!(detect(&dir.path().join("absent.PNG")), Some(MediaKind::Png));
        let unknown = dir.path().join("notes.txt");
        std::fs::write(&unknown, b"hello").unwrap();
        assert_eq!(detect(&unknown), None);
    }

    #[test]
    fn tiff_based_raw_is_decided_by_extension() {
        let dir = crate::testutil::TmpDir::new("media-raw");
        let dng = dir.path().join("IMG_0001.DNG");
        std::fs::copy(fixtures_dir().join("e.tiff"), &dng).unwrap();
        assert_eq!(detect(&dng), Some(MediaKind::Raw));
        let tif = dir.path().join("scan.tif");
        std::fs::copy(fixtures_dir().join("e.tiff"), &tif).unwrap();
        assert_eq!(detect(&tif), Some(MediaKind::Tiff));
    }

    #[test]
    fn ftyp_brand_separates_heif_from_video() {
        let ftyp = |brand: &[u8; 4]| {
            let mut v = vec![0, 0, 0, 24];
            v.extend_from_slice(b"ftyp");
            v.extend_from_slice(brand);
            v.extend_from_slice(&[0; 12]);
            v
        };
        assert_eq!(sniff(&ftyp(b"heic")), Some(MediaKind::Heif));
        assert_eq!(sniff(&ftyp(b"avif")), Some(MediaKind::Heif));
        assert_eq!(sniff(&ftyp(b"mif1")), Some(MediaKind::Heif));
        assert_eq!(sniff(&ftyp(b"qt  ")), Some(MediaKind::Video));
        assert_eq!(sniff(&ftyp(b"isom")), Some(MediaKind::Video));
        assert_eq!(sniff(&ftyp(b"crx ")), Some(MediaKind::Raw));
    }

    #[test]
    fn jxl_has_two_signatures() {
        assert_eq!(sniff(&[0xFF, 0x0A, 0x00, 0x00]), Some(MediaKind::Jxl));
        assert_eq!(sniff(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A]), Some(MediaKind::Jxl));
    }

    #[test]
    fn riff_distinguishes_webp_from_avi() {
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some(MediaKind::Webp));
        assert_eq!(sniff(b"RIFF\x10\0\0\0AVI LIST"), Some(MediaKind::Video));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WAVEfmt "), None);
    }
}
