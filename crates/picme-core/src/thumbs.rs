//! Miniatures freedesktop (Thumbnail Managing Standard) : `<racine>/{large,normal}/<md5 de l'URI>.png`.
//!
//! La racine est un parametre (`$XDG_CACHE_HOME/thumbnails` en usage normal, voir `default_root`) : aucun `set_var`,
//! les tests travaillent dans un dossier jetable. On lit `large` puis `normal` ; on n'ecrit que `large` (256 px) ;
//! les videos ne sont jamais ecrites (pas de generation de miniature video en v1), seulement lues si le file
//! manager en a laisse une. Les chunks `tEXt` sont lus avant les donnees d'image, comme le font les ecrivains usuels.

use crate::decode::{Frame, PREVIEW_SIZE};
use crate::dir::Entry;
use crate::error::{Error, Result};
use crate::media::MediaKind;
use md5::{Digest, Md5};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use std::fmt::Write as _;
use std::io::{BufReader, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Tout est encode sauf les non reserves (`-._~`), les reserves permis dans un chemin (`!$&'()*+,;=:@`) et `/` :
/// le jeu de `g_file_get_uri` (G_URI_RESERVED_CHARS_ALLOWED_IN_PATH), verifie contre gio dans `crates/picme`.
const PATH_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/');

/// Ordre de lecture : la plus grande d'abord.
const SIZE_DIRS: [&str; 2] = ["large", "normal"];

/// URI `file://` d'un chemin absolu (un chemin relatif est rendu absolu sans resoudre les liens symboliques).
/// Les octets du nom sont encodes tels quels : un nom non UTF-8 reste representable.
pub fn uri(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", percent_encode(abs.as_os_str().as_encoded_bytes(), PATH_SET))
}

/// Racine par defaut : `$XDG_CACHE_HOME/thumbnails`, sinon `$HOME/.cache/thumbnails` (valeurs relatives ignorees,
/// comme le veut la spec XDG).
pub fn default_root() -> Option<PathBuf> {
    let absolute = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let cache = absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|h| h.join(".cache")))?;
    Some(cache.join("thumbnails"))
}

/// Nom de fichier d'une miniature : md5 hexadecimal minuscule de l'URI, suivi de `.png`.
fn file_name(uri: &str) -> String {
    let digest = Md5::digest(uri.as_bytes());
    let mut name = String::with_capacity(36);
    for b in digest.iter() {
        let _ = write!(name, "{b:02x}");
    }
    name.push_str(".png");
    name
}

/// Chemin de la miniature de `uri` dans le sous-dossier `size_dir` de `root`.
fn thumb_path(root: &Path, size_dir: &str, uri: &str) -> PathBuf {
    root.join(size_dir).join(file_name(uri))
}

/// Mtime en secondes entieres, la seule precision que la spec garde (`Thumb::MTime`).
fn mtime_secs(entry: &Entry) -> i64 {
    entry.mtime_ns.div_euclid(1_000_000_000)
}

/// Cache de miniatures disque. Sans etat : une racine, et des operations qui ne partagent rien (utilisable depuis
/// plusieurs threads, l'ecriture est atomique par `rename`).
#[derive(Debug, Clone)]
pub struct ThumbStore {
    root: PathBuf,
}

impl ThumbStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ThumbStore { root: root.into() }
    }

    /// Miniature valide de `entry`, ou `None` (absente, illisible, ou perimee : `Thumb::MTime` different du mtime
    /// du fichier, ou `Thumb::URI` / `Thumb::Size` incoherents). Ne leve jamais d'erreur : une miniature qu'on
    /// ne sait pas lire se regenere.
    pub fn read(&self, entry: &Entry) -> Option<Frame> {
        let uri = uri(&entry.path);
        SIZE_DIRS.iter().find_map(|dir| read_png(&thumb_path(&self.root, dir, &uri), &uri, entry))
    }

    /// Ecrit la miniature `large` de `entry`. `frame` doit tenir dans 256 x 256 (venir de `Target::Fit(256, 256)`) ;
    /// `frame.full_w` x `frame.full_h` deviennent `Thumb::Image::Width` et `Height`. Refuse les videos.
    pub fn store(&self, entry: &Entry, frame: &Frame) -> Result<()> {
        if entry.kind == MediaKind::Video {
            return Err(Error::Unsupported("miniature de video".into()));
        }
        let max = PREVIEW_SIZE;
        if frame.w == 0 || frame.h == 0 || frame.w > max || frame.h > max {
            return Err(Error::Invalid(format!("miniature {}x{} hors de {max}x{max}", frame.w, frame.h)));
        }
        if frame.rgba.len() != frame.w as usize * frame.h as usize * 4 {
            return Err(Error::Invalid("buffer RGBA incoherent avec les dimensions".into()));
        }
        let uri = uri(&entry.path);
        let png = encode_png(&uri, entry, frame)?;

        let dir = self.root.join("large");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let target = dir.join(file_name(&uri));
        // meme dossier que la cible (rename atomique), nom unique par process et par appel (deux threads)
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(&png))
            .and_then(|()| std::fs::rename(&tmp, &target));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
}

fn encode_png(uri: &str, entry: &Entry, frame: &Frame) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.rgba.len() / 2);
    let mut enc = png::Encoder::new(&mut out, frame.w, frame.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    for (key, value) in [
        ("Thumb::URI", uri.to_string()),
        ("Thumb::MTime", mtime_secs(entry).to_string()),
        ("Thumb::Size", entry.size.to_string()),
        ("Thumb::Image::Width", frame.full_w.to_string()),
        ("Thumb::Image::Height", frame.full_h.to_string()),
    ] {
        enc.add_text_chunk(key.to_string(), value).map_err(Error::decode)?;
    }
    let mut writer = enc.write_header().map_err(Error::decode)?;
    writer.write_image_data(&frame.rgba).map_err(Error::decode)?;
    writer.finish().map_err(Error::decode)?;
    Ok(out)
}

/// Decode un PNG de miniature et le valide contre `entry`.
fn read_png(path: &Path, uri: &str, entry: &Entry) -> Option<Frame> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;

    let text = |key: &str| reader.info().uncompressed_latin1_text.iter().find(|c| c.keyword == key).map(|c| c.text.trim().to_string());
    if text("Thumb::MTime")?.parse::<i64>().ok()? != mtime_secs(entry) {
        return None;
    }
    if text("Thumb::URI").is_some_and(|u| u != uri) {
        return None;
    }
    if text("Thumb::Size").is_some_and(|s| s.parse::<u64>().ok() != Some(entry.size)) {
        return None;
    }
    let dim = |key: &str| text(key).and_then(|v| v.parse::<u32>().ok()).filter(|&d| d > 0);
    let (full_w, full_h) = (dim("Thumb::Image::Width"), dim("Thumb::Image::Height"));

    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some(Frame { rgba, w, h, full_w: full_w.unwrap_or(w), full_h: full_h.unwrap_or(h) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Target, decode};
    use crate::dir::stat_entry;
    use crate::fixtures_dir;
    use crate::testutil::TmpDir;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    /// Copie `a.jpg` sous `dir/name` et rend son `Entry` (chemin absolu et stable, contrairement a `fixtures_dir`).
    fn copy_fixture(dir: &Path, name: &str) -> Entry {
        let path = dir.join(name);
        std::fs::copy(fixtures_dir().join("a.jpg"), &path).unwrap();
        stat_entry(&std::path::absolute(&path).unwrap()).unwrap()
    }

    #[test]
    fn uri_encodes_like_a_file_uri() {
        assert_eq!(uri(Path::new("/home/jens/photos/me.png")), "file:///home/jens/photos/me.png");
        assert_eq!(uri(Path::new("/a b/é#%?+[x].jpg")), "file:///a%20b/%C3%A9%23%25%3F+%5Bx%5D.jpg");
        assert_eq!(uri(Path::new("/x/!$&'()*,;=:@~-_.jpg")), "file:///x/!$&'()*,;=:@~-_.jpg");
        // nom non UTF-8 : ses octets sont encodes tels quels
        let raw = std::ffi::OsStr::from_bytes(b"/x/\xff.jpg");
        assert_eq!(uri(Path::new(raw)), "file:///x/%FF.jpg");
    }

    #[test]
    fn file_name_is_the_md5_of_the_uri() {
        // exemple de la spec freedesktop
        assert_eq!(file_name("file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697.png");
        let p = thumb_path(Path::new("/cache"), "large", "file:///home/jens/photos/me.png");
        assert_eq!(p, Path::new("/cache/large/c6ee772d9e49320e97ec29a7eb5b1697.png"));
    }

    #[test]
    fn default_root_is_absolute() {
        // pas de set_var (process multi-thread) : seule la forme du resultat est testee
        if let Some(root) = default_root() {
            assert!(root.is_absolute() && root.ends_with("thumbnails"));
        }
    }

    #[test]
    fn store_then_read_round_trips() {
        let tmp = TmpDir::new("thumbs-roundtrip");
        let entry = copy_fixture(tmp.path(), "photo é #1.jpg");
        let store = ThumbStore::new(tmp.path().join("thumbnails"));
        assert!(store.read(&entry).is_none());

        let frame = decode(&entry.path, Target::Fit(256, 256)).unwrap();
        assert_eq!((frame.w, frame.h, frame.full_w, frame.full_h), (256, 192, 640, 480));
        store.store(&entry, &frame).unwrap();

        let back = store.read(&entry).expect("thumbnail valide");
        assert_eq!((back.w, back.h, back.full_w, back.full_h), (256, 192, 640, 480));
        assert_eq!(back.rgba, frame.rgba);
        // le fichier porte le nom md5 de l'URI
        let named = tmp.path().join("thumbnails/large").join(file_name(&uri(&entry.path)));
        assert!(named.is_file(), "{}", named.display());
        // aucun temporaire ne reste
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("thumbnails/large")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn stored_text_chunks_follow_the_spec() {
        let tmp = TmpDir::new("thumbs-chunks");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let store = ThumbStore::new(tmp.path().join("t"));
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap()).unwrap();

        let file = std::fs::File::open(tmp.path().join("t/large").join(file_name(&uri(&entry.path)))).unwrap();
        let reader = png::Decoder::new(BufReader::new(file)).read_info().unwrap();
        let get = |k: &str| reader.info().uncompressed_latin1_text.iter().find(|c| c.keyword == k).map(|c| c.text.clone());
        assert_eq!(get("Thumb::URI").unwrap(), uri(&entry.path));
        assert_eq!(get("Thumb::MTime").unwrap(), (entry.mtime_ns / 1_000_000_000).to_string());
        assert_eq!(get("Thumb::Size").unwrap(), entry.size.to_string());
        assert_eq!(get("Thumb::Image::Width").unwrap(), "640");
        assert_eq!(get("Thumb::Image::Height").unwrap(), "480");
    }

    #[test]
    fn permissions_are_private() {
        let tmp = TmpDir::new("thumbs-perms");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("fresh/thumbnails");
        let store = ThumbStore::new(&root);
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap()).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("large")), 0o700);
        assert_eq!(mode(&root.join("large").join(file_name(&uri(&entry.path)))), 0o600);
    }

    #[test]
    fn stale_thumbnail_is_ignored_after_mtime_or_size_change() {
        let tmp = TmpDir::new("thumbs-stale");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let store = ThumbStore::new(tmp.path().join("t"));
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap()).unwrap();
        assert!(store.read(&entry).is_some());

        // meme fichier, mtime d'une heure plus tard
        let later = SystemTime::now() + Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&entry.path).unwrap().set_modified(later).unwrap();
        let touched = stat_entry(&entry.path).unwrap();
        assert!(store.read(&touched).is_none());

        // un sous-seconde ne change rien : la spec ne garde que les secondes
        let same_second = Entry { mtime_ns: entry.mtime_ns + 1, ..entry.clone() };
        if same_second.mtime_ns / 1_000_000_000 == entry.mtime_ns / 1_000_000_000 {
            assert!(store.read(&same_second).is_some());
        }
        // taille differente
        let resized = Entry { size: entry.size + 1, ..entry.clone() };
        assert!(store.read(&resized).is_none());
    }

    #[test]
    fn read_falls_back_to_normal_and_ignores_foreign_uri() {
        let tmp = TmpDir::new("thumbs-normal");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        let store = ThumbStore::new(&root);
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap()).unwrap();

        let name = file_name(&uri(&entry.path));
        std::fs::create_dir_all(root.join("normal")).unwrap();
        std::fs::rename(root.join("large").join(&name), root.join("normal").join(&name)).unwrap();
        assert!(store.read(&entry).is_some(), "repli sur normal");

        // une miniature dont Thumb::URI est celle d'un autre fichier (collision de nom) n'est pas reprise
        let other = Entry { path: entry.path.with_file_name("autre.jpg"), ..entry.clone() };
        std::fs::copy(root.join("normal").join(&name), root.join("normal").join(file_name(&uri(&other.path)))).unwrap();
        assert!(store.read(&other).is_none());
    }

    #[test]
    fn store_refuses_videos_and_oversized_frames() {
        let tmp = TmpDir::new("thumbs-refuse");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let store = ThumbStore::new(tmp.path().join("t"));
        let frame = decode(&entry.path, Target::Fit(256, 256)).unwrap();

        let video = Entry { kind: MediaKind::Video, ..entry.clone() };
        assert!(matches!(store.store(&video, &frame), Err(Error::Unsupported(_))));
        let big = decode(&entry.path, Target::Fit(300, 300)).unwrap();
        assert!(matches!(store.store(&entry, &big), Err(Error::Invalid(_))));
        assert!(!tmp.path().join("t").exists(), "rien n'est cree en cas de refus");
    }

    #[test]
    fn unreadable_thumbnail_is_none_not_a_panic() {
        let tmp = TmpDir::new("thumbs-corrupt");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        std::fs::create_dir_all(root.join("large")).unwrap();
        std::fs::write(root.join("large").join(file_name(&uri(&entry.path))), b"pas un png").unwrap();
        assert!(ThumbStore::new(&root).read(&entry).is_none());
    }
}
