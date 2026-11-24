//! Detection du type de media : magic bytes d'abord, extension en repli. Le listing d'un dossier se fie a l'extension
//! quand elle est connue (aucune ouverture), et lit `HEAD_LEN` octets des fichiers dont l'extension est inconnue ou
//! absente (`sniff_path`), sauf les extensions qui ne sont manifestement pas des medias (`is_non_media_extension`).

use std::fs::File;
use std::io::{self, Read};
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
    /// Premiere image seulement.
    Gif,
    Bmp,
    Ico,
    Qoi,
    /// PBM, PGM, PPM, PAM.
    Pnm,
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
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some(MediaKind::Gif);
    }
    if head.starts_with(b"qoif") {
        return Some(MediaKind::Qoi);
    }
    // BMP : "BM", deux champs reserves a zero, puis la taille d'un des en-tetes DIB connus
    if head.len() >= 18 && head.starts_with(b"BM") && head[6..10] == [0; 4] {
        let dib = u32::from_le_bytes(head[14..18].try_into().unwrap());
        if matches!(dib, 12 | 40 | 52 | 56 | 64 | 108 | 124) {
            return Some(MediaKind::Bmp);
        }
    }
    // ICO et CUR : reserve 0, type 1 ou 2, au moins une image, octet reserve a zero de la premiere entree
    if head.len() >= 16 && head[0..2] == [0, 0] && matches!(head[2..4], [1 | 2, 0]) && head[4] > 0 && head[5] == 0 && head[9] == 0 {
        return Some(MediaKind::Ico);
    }
    // PNM : P1 a P6 (et P7, PAM), une espace, puis un commentaire ou un chiffre
    if head.len() >= 3 && head[0] == b'P' && (b'1'..=b'7').contains(&head[1]) && matches!(head[2], b' ' | b'\t' | b'\r' | b'\n') {
        let next = head[3..].iter().find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
        if next.is_some_and(|b| *b == b'#' || b.is_ascii_digit() || (head[1] == b'7' && *b == b'W')) {
            return Some(MediaKind::Pnm);
        }
    }
    // Matroska et WebM, MPEG program stream (pack et sequence header), ASF/WMV, FLV.
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
        || head.starts_with(&[0, 0, 1, 0xBA])
        || head.starts_with(&[0, 0, 1, 0xB3])
        || head.starts_with(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11])
        || head.starts_with(b"FLV\x01")
    {
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
        // audio seul
        if [b"M4A ", b"M4B ", b"M4P ", b"F4A "].contains(&major) {
            return None;
        }
        return Some(MediaKind::Video);
    }
    None
}

/// Extensions de fichiers qui ne sont manifestement pas des medias : le listing ne les ouvre pas pour en lire la tete.
pub fn is_non_media_extension(ext: &str) -> bool {
    const NON_MEDIA: &[&str] = &[
        "txt", "md", "rtf", "log", "csv", "json", "xml", "html", "htm", "css", "js", "ini", "cfg", "conf", "yml", "yaml", "toml", "db", "sqlite",
        "plist", "ds_store", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "zip", "gz", "tgz", "bz2", "xz", "7z", "rar",
        "tar", "iso", "dmg", "exe", "dll", "so", "lnk", "url", "desktop", "nfo", "srt", "sub", "xmp", "aae", "thm", "mp3", "m4a", "aac", "wav",
        "flac", "ogg", "opus", "wma", "py", "rs", "sh", "php", "c", "h",
    ];
    NON_MEDIA.iter().any(|n| ext.eq_ignore_ascii_case(n))
}

/// Type d'apres les premiers octets du fichier. `Ok(None)` : aucune signature connue.
pub fn sniff_path(path: &Path) -> io::Result<Option<MediaKind>> {
    let mut head = [0u8; HEAD_LEN];
    let mut f = File::open(path)?;
    let mut n = 0;
    // `read` peut rendre moins que demande : on lit jusqu'a HEAD_LEN ou la fin du fichier
    while n < HEAD_LEN {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(sniff(&head[..n]))
}

/// Type d'apres l'extension (insensible a la casse). `None` pour une extension inconnue.
pub fn kind_from_extension(ext: &str) -> Option<MediaKind> {
    let kind = match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" | "jif" => MediaKind::Jpeg,
        "png" | "apng" => MediaKind::Png,
        "webp" => MediaKind::Webp,
        "tif" | "tiff" => MediaKind::Tiff,
        "heic" | "heif" | "hif" | "avif" | "heics" | "heifs" | "avci" | "avcs" | "avifs" => MediaKind::Heif,
        "jxl" => MediaKind::Jxl,
        "gif" => MediaKind::Gif,
        "bmp" | "dib" => MediaKind::Bmp,
        "ico" | "cur" => MediaKind::Ico,
        "qoi" => MediaKind::Qoi,
        "pbm" | "pgm" | "ppm" | "pnm" | "pam" => MediaKind::Pnm,
        "dng" | "cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "sr2" | "srf" | "orf" | "rw2" | "raf" | "pef" | "srw" | "mrw" | "erf" | "kdc"
        | "dcr" | "3fr" | "mef" | "mos" | "iiq" | "rwl" => MediaKind::Raw,
        "mov" | "qt" | "mp4" | "m4v" | "f4v" | "mkv" | "webm" | "avi" | "3gp" | "3g2" | "mpg" | "mpeg" | "mpe" | "m2v" | "mts" | "m2ts" | "vob"
        | "wmv" | "asf" | "flv" | "ogv" | "divx" => MediaKind::Video,
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
    fn simple_signatures_and_what_they_do_not_match() {
        assert_eq!(sniff(b"GIF89a\x01\0\x01\0"), Some(MediaKind::Gif));
        assert_eq!(sniff(b"GIF87a\x01\0\x01\0"), Some(MediaKind::Gif));
        assert_eq!(sniff(b"qoif\0\0\0\x10\0\0\0\x10\x03\0"), Some(MediaKind::Qoi));
        let mut bmp = b"BM\x36\0\0\0\0\0\0\0\x36\0\0\0".to_vec();
        bmp.extend_from_slice(&40u32.to_le_bytes());
        assert_eq!(sniff(&bmp), Some(MediaKind::Bmp));
        // "BM" d'un texte, ou d'un en-tete DIB inconnu : pas un BMP
        assert_eq!(sniff(b"BM est une marque, pas un bitmap"), None);
        assert_eq!(sniff(b"P6\n# c\n2 2\n255\n"), Some(MediaKind::Pnm));
        assert_eq!(sniff(b"P5 2 2 255 "), Some(MediaKind::Pnm));
        assert_eq!(sniff(b"P7\nWIDTH 2\n"), Some(MediaKind::Pnm));
        // du texte qui commence par P1 ou P6 n'est pas une image PNM
        assert_eq!(sniff(b"P6 est la touche"), None);
        assert_eq!(sniff(b"Plan de travail"), None);
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0, 16, 16, 0, 0, 1, 0, 32, 0, 0, 0]), Some(MediaKind::Ico));
        assert_eq!(sniff(&[0; 16]), None);
        assert_eq!(sniff(&[0, 0, 1, 0xBA, 0x44, 0, 4, 0]), Some(MediaKind::Video));
        assert_eq!(sniff(b"\x1A\x45\xDF\xA3rest"), Some(MediaKind::Video));
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"juste du texte"), None);
        // m4a : de l'audio, pas une video
        assert_eq!(sniff(b"\0\0\0\x20ftypM4A \0\0\0\0"), None);
    }

    #[test]
    fn extensions_cover_the_usual_variants_and_refuse_the_obvious_non_media() {
        for (ext, kind) in [
            ("JPE", MediaKind::Jpeg),
            ("gif", MediaKind::Gif),
            ("BMP", MediaKind::Bmp),
            ("ico", MediaKind::Ico),
            ("qoi", MediaKind::Qoi),
            ("ppm", MediaKind::Pnm),
            ("crw", MediaKind::Raw),
            ("mpg", MediaKind::Video),
            ("wmv", MediaKind::Video),
        ] {
            assert_eq!(kind_from_extension(ext), Some(kind), "{ext}");
        }
        assert_eq!(kind_from_extension("ithmb"), None);
        for ext in ["txt", "DB", "ini", "json", "xml", "html", "pdf", "zip", "DS_Store"] {
            assert!(is_non_media_extension(ext), "{ext}");
            assert_eq!(kind_from_extension(ext), None, "{ext}");
        }
        assert!(!is_non_media_extension("ithmb") && !is_non_media_extension("jpg") && !is_non_media_extension("dat"));
    }

    #[test]
    fn sniff_path_reads_the_head_and_reports_unreadable_files() {
        let dir = crate::testutil::TmpDir::new("media-sniff-path");
        let p = dir.path().join("T984.ithmb");
        std::fs::copy(fixtures_dir().join("a.jpg"), &p).unwrap();
        assert_eq!(sniff_path(&p).unwrap(), Some(MediaKind::Jpeg));
        let txt = dir.path().join("notes");
        std::fs::write(&txt, b"du texte").unwrap();
        assert_eq!(sniff_path(&txt).unwrap(), None);
        std::fs::write(&txt, b"").unwrap();
        assert_eq!(sniff_path(&txt).unwrap(), None);
        assert_eq!(sniff_path(&dir.path().join("absent")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn riff_distinguishes_webp_from_avi() {
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some(MediaKind::Webp));
        assert_eq!(sniff(b"RIFF\x10\0\0\0AVI LIST"), Some(MediaKind::Video));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WAVEfmt "), None);
    }
}
