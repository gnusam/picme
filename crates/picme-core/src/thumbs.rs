//! Miniatures freedesktop (Thumbnail Managing Standard) : `<racine>/{normal,large,x-large,xx-large}/<md5 de l'URI>.png`,
//! partagees avec les file managers (Nautilus, Thunar, Dolphin, pcmanfm, gThumb).
//!
//! La racine est un parametre (`$XDG_CACHE_HOME/thumbnails` en usage normal, voir `default_root`) : aucun `set_var`,
//! les tests travaillent dans un dossier jetable.
//!
//! Lecture : pour une taille voulue, la plus petite des quatre tailles qui suffit, sinon la plus grande disponible
//! (`Frame::low_res` le dit : une miniature n'est jamais agrandie). Une miniature n'est reprise que si `Thumb::MTime`
//! (et `Thumb::Size` et `Thumb::URI` quand ils sont la) correspondent au fichier. `~/.thumbnails` (ancien emplacement,
//! lecture seule) n'est consulte que si la racine n'a aucune miniature valide du fichier. Un marqueur `fail/<appli>/`
//! d'un autre programme est lisible (`marked_failed`) : l'appelant ne genere alors pas de miniature.
//!
//! Ecriture : `normal` et `large` seulement (les autres programmes gerent mieux les grandes), par fichier temporaire
//! puis `rename`, jamais par-dessus une miniature valide, jamais pour une video (pas de generation de miniature
//! video en v1, seulement lecture de celles du file manager). Les chunks `tEXt` sont lus avant les donnees d'image,
//! comme le font les ecrivains usuels : le choix d'une taille ne decode que la miniature retenue.

use crate::decode::{self, Frame, ratio_matches};
use crate::dir::Entry;
use crate::error::{Error, Result};
use crate::media::MediaKind;
use md5::{Digest, Md5};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use std::fmt::Write as _;
use std::io::{BufReader, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Tout est encode sauf les non reserves (`-._~`), `!$&'()*+,=:@` et `/` : le jeu de `g_file_get_uri`, verifie
/// octet par octet contre gio dans `crates/picme/tests/uri_gio.rs`. `;` est encode (%3B) par gio, bien qu'il soit
/// un sous-delimiteur permis dans un chemin par la RFC 3986.
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
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/');

/// Les quatre tailles du standard. Chacune est une boite carree dans laquelle la miniature tient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ThumbSize {
    Normal,
    Large,
    XLarge,
    XXLarge,
}

impl ThumbSize {
    pub const ALL: [ThumbSize; 4] = [ThumbSize::Normal, ThumbSize::Large, ThumbSize::XLarge, ThumbSize::XXLarge];

    /// Cote de la boite : 128, 256, 512, 1024.
    pub const fn px(self) -> u32 {
        match self {
            ThumbSize::Normal => 128,
            ThumbSize::Large => 256,
            ThumbSize::XLarge => 512,
            ThumbSize::XXLarge => 1024,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            ThumbSize::Normal => "normal",
            ThumbSize::Large => "large",
            ThumbSize::XLarge => "x-large",
            ThumbSize::XXLarge => "xx-large",
        }
    }

    /// Tailles que picme ecrit.
    pub const fn writable(self) -> bool {
        matches!(self, ThumbSize::Normal | ThumbSize::Large)
    }
}

/// Taille voulue (grand cote, en pixels) du `Preview` du controller : la plus grande miniature disponible.
pub const PREVIEW_WANT: u32 = ThumbSize::XXLarge.px();

/// Ordre d'essai pour `want` : les tailles qui suffisent, de la plus petite a la plus grande, puis le repli, de la
/// plus grande a la plus petite des insuffisantes.
fn size_order(want: u32) -> [ThumbSize; 4] {
    let mut sizes = ThumbSize::ALL;
    sizes.sort_by_key(|s| if s.px() >= want { (0, s.px() as i64) } else { (1, -(s.px() as i64)) });
    sizes
}

/// URI `file://` d'un chemin. Comme `g_file_new_for_path`, le chemin est rendu absolu (relatif au dossier courant)
/// puis normalise lexicalement (`.`, `..`, `//`, `/` final), sans resoudre les liens symboliques : le file manager
/// nomme ses miniatures par cette forme, pas par celle que l'utilisateur a tapee. Les octets du nom sont encodes
/// tels quels : un nom non UTF-8 reste representable.
pub fn uri(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut parts: Vec<&OsStr> = Vec::new();
    for c in abs.components() {
        match c {
            Component::Normal(name) => parts.push(name),
            // `..` au-dessus de la racine reste a la racine
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut bytes = Vec::new();
    for name in &parts {
        bytes.push(b'/');
        bytes.extend_from_slice(name.as_encoded_bytes());
    }
    if bytes.is_empty() {
        bytes.push(b'/');
    }
    format!("file://{}", percent_encode(&bytes, PATH_SET))
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

/// Cache de miniatures disque. Sans etat : une racine (et l'ancien `~/.thumbnails` en lecture seule), des operations
/// qui ne partagent rien (utilisable depuis plusieurs threads, l'ecriture est atomique par `rename`).
#[derive(Debug, Clone)]
pub struct ThumbStore {
    root: PathBuf,
    legacy: Option<PathBuf>,
}

impl ThumbStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ThumbStore { root: root.into(), legacy: None }
    }

    /// Ajoute un ancien dossier (`~/.thumbnails`), consulte en lecture seulement si la racine n'a pas de miniature
    /// valide du fichier.
    pub fn with_legacy(mut self, legacy: impl Into<PathBuf>) -> Self {
        self.legacy = Some(legacy.into());
        self
    }

    /// Le cache de l'utilisateur : `default_root`, et `$HOME/.thumbnails` s'il existe. `None` sans `HOME` ni
    /// `XDG_CACHE_HOME` absolu.
    pub fn system() -> Option<Self> {
        let store = ThumbStore::new(default_root()?);
        let legacy = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()).map(|h| h.join(".thumbnails"));
        Some(match legacy.filter(|p| p.is_dir()) {
            Some(legacy) => store.with_legacy(legacy),
            None => store,
        })
    }

    /// Chemin (existant ou non) de la miniature de `entry` dans le dossier `size` de la racine.
    pub fn path_of(&self, entry: &Entry, size: ThumbSize) -> PathBuf {
        thumb_path(&self.root, size.dir(), &uri(&entry.path))
    }

    /// Retire les miniatures normal et large de `path` (celles que picme ecrit), apres une modification ou une
    /// suppression : MTime est en secondes, une reecriture dans la meme seconde a la meme taille les laisserait
    /// valides. Les autres tailles appartiennent a d'autres programmes : elles ne sont pas touchees, la validation
    /// par MTime et Size les ecarte. Rend le nombre de fichiers retires.
    pub fn forget(&self, path: &Path) -> usize {
        let uri = uri(path);
        ThumbSize::ALL.iter().filter(|s| s.writable()).filter(|s| std::fs::remove_file(thumb_path(&self.root, s.dir(), &uri)).is_ok()).count()
    }

    /// La racine puis l'ancien dossier, dans l'ordre de consultation.
    fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.root.as_path()).chain(self.legacy.as_deref())
    }

    /// Miniature valide de `entry` pour une demande de `want` pixels de grand cote, ou `None` (absente, illisible,
    /// ou perimee : `Thumb::MTime` different du mtime du fichier, ou `Thumb::URI` / `Thumb::Size` incoherents, ou
    /// proportions differentes de celles du fichier). La plus petite taille qui atteint `want` est preferee, sinon
    /// la plus grande disponible, et `Frame::low_res` le dit : la miniature n'est jamais agrandie. `full_w` et
    /// `full_h` sont ceux du fichier (lus dans son en-tete, les chunks `Thumb::Image::*` etant facultatifs).
    /// Ne leve jamais d'erreur : une miniature qu'on ne sait pas lire se regenere.
    pub fn read(&self, entry: &Entry, want: u32) -> Option<Frame> {
        let uri = uri(&entry.path);
        self.roots().find_map(|root| {
            size_order(want).into_iter().find_map(|size| read_png(&thumb_path(root, size.dir(), &uri), &uri, entry, want))
        })
    }

    /// Vrai si un autre programme a marque `entry` en echec (`fail/<appli>/<md5>.png`, a jour : meme `Thumb::MTime`).
    /// L'appelant ne genere pas de miniature pour ce fichier ; picme n'ecrit jamais de marqueur lui-meme.
    pub fn marked_failed(&self, entry: &Entry) -> bool {
        let uri = uri(&entry.path);
        let name = file_name(&uri);
        self.roots().any(|root| {
            let apps = std::fs::read_dir(root.join("fail")).into_iter().flatten().flatten();
            apps.into_iter().any(|app| open_valid(&app.path().join(&name), &uri, entry).is_some())
        })
    }

    /// Ecrit les miniatures `large` puis `normal` d'une frame decodee en `Target::Preview` (256 px). Rien pour une
    /// image de moins de 128 px (la spec n'en veut pas). Les deux sont tentees, la premiere erreur est rendue.
    pub fn store_standard(&self, entry: &Entry, frame: Frame) -> Result<()> {
        if frame.full_w.max(frame.full_h) < ThumbSize::Normal.px() {
            return Ok(());
        }
        let large = self.store(entry, &frame, ThumbSize::Large);
        let normal = frame.shrink_to(ThumbSize::Normal.px(), ThumbSize::Normal.px()).and_then(|f| self.store(entry, &f, ThumbSize::Normal));
        large.and(normal).map(|_| ())
    }

    /// Ecrit la miniature de `entry` dans le dossier `size` (`normal` ou `large` seulement : les grandes tailles
    /// sont celles des file managers). `frame` doit tenir dans la boite de `size` ; `frame.full_w` x `frame.full_h`
    /// deviennent `Thumb::Image::Width` et `Height`. Refuse les videos. Rend `false` sans rien ecrire si une
    /// miniature valide est deja la (jamais ecrasee : une perimee ou illisible l'est). Entre le controle et le
    /// `rename`, un autre programme peut en poser une : elle est remplacee par une equivalente.
    pub fn store(&self, entry: &Entry, frame: &Frame, size: ThumbSize) -> Result<bool> {
        if !size.writable() {
            return Err(Error::Invalid(format!("picme n'ecrit pas la taille {}", size.dir())));
        }
        if entry.kind == MediaKind::Video {
            return Err(Error::Unsupported("miniature de video".into()));
        }
        let max = size.px();
        if frame.w == 0 || frame.h == 0 || frame.w > max || frame.h > max {
            return Err(Error::Invalid(format!("miniature {}x{} hors de {max}x{max}", frame.w, frame.h)));
        }
        if frame.rgba.len() != frame.w as usize * frame.h as usize * 4 {
            return Err(Error::Invalid("buffer RGBA incoherent avec les dimensions".into()));
        }
        let uri = uri(&entry.path);
        let dir = self.root.join(size.dir());
        let target = dir.join(file_name(&uri));
        if open_valid(&target, &uri, entry).is_some() {
            return Ok(false);
        }
        let png = encode_png(&uri, entry, frame)?;

        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
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
        Ok(true)
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

/// `Thumb::MTime` : secondes entieres, ou avec une partie fractionnaire comme l'ecrivent certains programmes
/// ("1514745613.000000", cas des miniatures de Nautilus) ; la spec ne garde que les secondes.
fn parse_mtime(text: &str) -> Option<i64> {
    text.trim().split('.').next()?.parse().ok()
}

/// PNG de miniature ouvert et valide contre `entry`, pixels pas encore lus. `dims` : `Thumb::Image::Width` x `Height`.
struct Opened {
    reader: png::Reader<BufReader<std::fs::File>>,
    dims: Option<(u32, u32)>,
}

/// Ouvre un PNG de miniature et lit ses chunks `tEXt` : `None` s'il est illisible ou perime (`Thumb::MTime`
/// obligatoire ; `Thumb::URI` et `Thumb::Size` controles quand ils sont la).
fn open_valid(path: &Path, uri: &str, entry: &Entry) -> Option<Opened> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let reader = decoder.read_info().ok()?;

    let info = reader.info();
    let text = |key: &str| info.uncompressed_latin1_text.iter().find(|c| c.keyword == key).map(|c| c.text.trim());
    if parse_mtime(text("Thumb::MTime")?)? != mtime_secs(entry) {
        return None;
    }
    if text("Thumb::URI").is_some_and(|u| u != uri) {
        return None;
    }
    if text("Thumb::Size").is_some_and(|s| s.parse::<u64>().ok() != Some(entry.size)) {
        return None;
    }
    let dim = |key: &str| text(key).and_then(|v| v.parse::<u32>().ok()).filter(|&d| d > 0);
    let dims = dim("Thumb::Image::Width").zip(dim("Thumb::Image::Height"));
    Some(Opened { reader, dims })
}

/// Dimensions de l'image entiere : les chunks `Thumb::Image::*` s'ils ont les proportions de la miniature, sinon
/// l'en-tete du fichier (les chunks sont facultatifs, et certains programmes y ecrivent les dimensions avant
/// rotation). `None` si le fichier a d'autres proportions que la miniature (tournee autrement) ; sans en-tete
/// lisible (JXL, RAW, video), la miniature fait foi.
fn source_dims(entry: &Entry, tags: Option<(u32, u32)>, (w, h): (u32, u32)) -> Option<(u32, u32)> {
    if let Some((fw, fh)) = tags
        && ratio_matches(w, h, fw, fh)
    {
        return Some((fw, fh));
    }
    match decode::probe_dims(&entry.path) {
        Some((fw, fh)) => ratio_matches(w, h, fw, fh).then_some((fw, fh)),
        None => Some((w, h)),
    }
}

/// Decode un PNG de miniature valide pour `entry`, `want` etant la taille demandee (pour `Frame::low_res`).
fn read_png(path: &Path, uri: &str, entry: &Entry, want: u32) -> Option<Frame> {
    let Opened { mut reader, dims } = open_valid(path, uri, entry)?;
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
    let (full_w, full_h) = source_dims(entry, dims, (w, h))?;
    // jamais agrandie : plus petite que demande alors que l'image a de quoi remplir la demande (a un pixel pres)
    let low_res = w.max(h) + 1 < want.min(full_w.max(full_h));
    Some(Frame { rgba, w, h, full_w, full_h, low_res })
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
        assert_eq!(uri(Path::new("/x/!$&'()*,=:@~-_;.jpg")), "file:///x/!$&'()*,=:@~-_%3B.jpg");
        // nom non UTF-8 : ses octets sont encodes tels quels
        let raw = std::ffi::OsStr::from_bytes(b"/x/\xff.jpg");
        assert_eq!(uri(Path::new(raw)), "file:///x/%FF.jpg");
    }

    #[test]
    fn uri_normalises_lexically_without_following_symlinks() {
        assert_eq!(uri(Path::new("/a/../b.jpg")), "file:///b.jpg");
        assert_eq!(uri(Path::new("/a/./b//c.jpg")), "file:///a/b/c.jpg");
        assert_eq!(uri(Path::new("/../x.jpg")), "file:///x.jpg");
        assert_eq!(uri(Path::new("/a/b/")), "file:///a/b");
        assert_eq!(uri(Path::new("/")), "file:///");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(uri(Path::new("x.jpg")), uri(&cwd.join("x.jpg")));
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
        assert!(store.read(&entry, 256).is_none());

        let frame = decode(&entry.path, Target::Fit(256, 256)).unwrap();
        assert_eq!((frame.w, frame.h, frame.full_w, frame.full_h), (256, 192, 640, 480));
        assert!(store.store(&entry, &frame, ThumbSize::Large).unwrap());

        let back = store.read(&entry, 256).expect("thumbnail valide");
        assert_eq!((back.w, back.h, back.full_w, back.full_h, back.low_res), (256, 192, 640, 480, false));
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
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap(), ThumbSize::Large).unwrap();

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
        store.store_standard(&entry, decode(&entry.path, Target::Preview).unwrap()).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        for dir in ["large", "normal"] {
            assert_eq!(mode(&root.join(dir)), 0o700, "{dir}");
            assert_eq!(mode(&root.join(dir).join(file_name(&uri(&entry.path)))), 0o600, "{dir}");
        }
    }

    #[test]
    fn stale_thumbnail_is_ignored_after_mtime_or_size_change() {
        let tmp = TmpDir::new("thumbs-stale");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let store = ThumbStore::new(tmp.path().join("t"));
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap(), ThumbSize::Large).unwrap();
        assert!(store.read(&entry, 256).is_some());

        // meme fichier, mtime d'une heure plus tard
        let later = SystemTime::now() + Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&entry.path).unwrap().set_modified(later).unwrap();
        let touched = stat_entry(&entry.path).unwrap();
        assert!(store.read(&touched, 256).is_none());

        // un sous-seconde ne change rien : la spec ne garde que les secondes
        let same_second = Entry { mtime_ns: entry.mtime_ns + 1, ..entry.clone() };
        if same_second.mtime_ns / 1_000_000_000 == entry.mtime_ns / 1_000_000_000 {
            assert!(store.read(&same_second, 256).is_some());
        }
        // taille differente
        let resized = Entry { size: entry.size + 1, ..entry.clone() };
        assert!(store.read(&resized, 256).is_none());
    }

    #[test]
    fn read_falls_back_to_normal_and_ignores_foreign_uri() {
        let tmp = TmpDir::new("thumbs-normal");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        let store = ThumbStore::new(&root);
        store.store(&entry, &decode(&entry.path, Target::Fit(256, 256)).unwrap(), ThumbSize::Large).unwrap();

        let name = file_name(&uri(&entry.path));
        std::fs::create_dir_all(root.join("normal")).unwrap();
        std::fs::rename(root.join("large").join(&name), root.join("normal").join(&name)).unwrap();
        assert!(store.read(&entry, 256).is_some(), "repli sur normal");

        // une miniature dont Thumb::URI est celle d'un autre fichier (collision de nom) n'est pas reprise
        let other = Entry { path: entry.path.with_file_name("autre.jpg"), ..entry.clone() };
        std::fs::copy(root.join("normal").join(&name), root.join("normal").join(file_name(&uri(&other.path)))).unwrap();
        assert!(store.read(&other, 256).is_none());
    }

    #[test]
    fn forget_removes_only_the_sizes_picme_writes() {
        let tmp = TmpDir::new("thumbs-forget");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let store = ThumbStore::new(tmp.path().join("t"));
        store.store_standard(&entry, decode(&entry.path, Target::Fit(256, 256)).unwrap()).unwrap();
        let xl = store.path_of(&entry, ThumbSize::XLarge);
        std::fs::create_dir_all(xl.parent().unwrap()).unwrap();
        std::fs::copy(store.path_of(&entry, ThumbSize::Large), &xl).unwrap();
        assert_eq!(store.forget(&entry.path), 2);
        assert!(!store.path_of(&entry, ThumbSize::Normal).exists() && !store.path_of(&entry, ThumbSize::Large).exists());
        assert!(xl.exists(), "x-large d'un autre programme : pas touche");
        assert_eq!(store.forget(&entry.path), 0);
    }

    #[test]
    fn store_refuses_videos_foreign_sizes_and_oversized_frames() {
        let tmp = TmpDir::new("thumbs-refuse");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let store = ThumbStore::new(tmp.path().join("t"));
        let frame = decode(&entry.path, Target::Fit(256, 256)).unwrap();

        let video = Entry { kind: MediaKind::Video, ..entry.clone() };
        assert!(matches!(store.store(&video, &frame, ThumbSize::Large), Err(Error::Unsupported(_))));
        let big = decode(&entry.path, Target::Fit(300, 300)).unwrap();
        assert!(matches!(store.store(&entry, &big, ThumbSize::Large), Err(Error::Invalid(_))));
        // 256 px ne tient pas dans `normal`
        assert!(matches!(store.store(&entry, &frame, ThumbSize::Normal), Err(Error::Invalid(_))));
        // x-large et xx-large sont ceux des file managers
        for size in [ThumbSize::XLarge, ThumbSize::XXLarge] {
            assert!(matches!(store.store(&entry, &frame, size), Err(Error::Invalid(_))), "{size:?}");
        }
        assert!(!tmp.path().join("t").exists(), "rien n'est cree en cas de refus");
    }

    #[test]
    fn unreadable_thumbnail_is_none_not_a_panic() {
        let tmp = TmpDir::new("thumbs-corrupt");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        std::fs::create_dir_all(root.join("large")).unwrap();
        std::fs::write(root.join("large").join(file_name(&uri(&entry.path))), b"pas un png").unwrap();
        assert!(ThumbStore::new(&root).read(&entry, 256).is_none());
    }

    /// Chunks d'une miniature ecrite par un autre programme : MTime fractionnaire, ni `Thumb::Size` ni `Thumb::Image::*`.
    fn foreign_text(entry: &Entry) -> Vec<(&'static str, String)> {
        vec![("Thumb::URI", uri(&entry.path)), ("Thumb::MTime", format!("{}.000000", mtime_secs(entry)))]
    }

    /// Pose une miniature RGB de `w` x `h`, gris `shade`, dans `<root>/<dir>/` sous le nom de `entry`.
    fn put_png(root: &Path, dir: &str, entry: &Entry, (w, h): (u32, u32), shade: u8, text: &[(&'static str, String)]) -> PathBuf {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        let path = root.join(dir).join(file_name(&uri(&entry.path)));
        let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        for (k, v) in text {
            enc.add_text_chunk(k.to_string(), v.clone()).unwrap();
        }
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&vec![shade; w as usize * h as usize * 3]).unwrap();
        writer.finish().unwrap();
        path
    }

    /// Cache de quatre tailles pour `a.jpg` (640x480) ; la xx-large est l'image entiere, jamais agrandie.
    fn seed(root: &Path, entry: &Entry, dirs: &[&str]) {
        for dir in dirs {
            let dims = match *dir {
                "normal" => (128, 96),
                "large" => (256, 192),
                "x-large" => (512, 384),
                _ => (640, 480),
            };
            put_png(root, dir, entry, dims, 200, &foreign_text(entry));
        }
    }

    #[test]
    fn size_order_prefers_the_smallest_that_suffices_then_the_largest_below() {
        use ThumbSize::*;
        assert_eq!(size_order(100), [Normal, Large, XLarge, XXLarge]);
        assert_eq!(size_order(128), [Normal, Large, XLarge, XXLarge]);
        assert_eq!(size_order(129), [Large, XLarge, XXLarge, Normal]);
        assert_eq!(size_order(300), [XLarge, XXLarge, Large, Normal]);
        assert_eq!(size_order(1024), [XXLarge, XLarge, Large, Normal]);
        assert_eq!(size_order(5000), [XXLarge, XLarge, Large, Normal]);
    }

    #[test]
    fn read_picks_the_smallest_sufficient_size_and_never_enlarges() {
        let tmp = TmpDir::new("thumbs-sizes");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        seed(&root, &entry, &["normal", "large", "x-large", "xx-large"]);
        let store = ThumbStore::new(&root);

        let got = |want: u32| store.read(&entry, want).map(|f| (f.w, f.h, f.full_w, f.full_h, f.low_res));
        // dimensions entieres lues dans le fichier : la miniature ne porte pas Thumb::Image::*
        assert_eq!(got(100), Some((128, 96, 640, 480, false)));
        assert_eq!(got(128), Some((128, 96, 640, 480, false)));
        assert_eq!(got(129), Some((256, 192, 640, 480, false)));
        assert_eq!(got(256), Some((256, 192, 640, 480, false)));
        assert_eq!(got(300), Some((512, 384, 640, 480, false)));
        // 1024 demande, l'image n'a que 640 px : sa xx-large (640x480) est l'image entiere, pas de low_res
        assert_eq!(got(1024), Some((640, 480, 640, 480, false)));

        // sans les grandes : repli sur la plus grande disponible, signale
        let small = tmp.path().join("small");
        seed(&small, &entry, &["normal", "large"]);
        let store = ThumbStore::new(&small);
        let got = |want: u32| store.read(&entry, want).map(|f| (f.w, f.h, f.low_res));
        assert_eq!(got(1024), Some((256, 192, true)));
        assert_eq!(got(256), Some((256, 192, false)));
        assert_eq!(got(512), Some((256, 192, true)));
        assert_eq!(got(128), Some((128, 96, false)));
        // rien que la normal : repli sur elle, signale
        let only_normal = tmp.path().join("normal-only");
        seed(&only_normal, &entry, &["normal"]);
        assert_eq!(ThumbStore::new(&only_normal).read(&entry, 1024).map(|f| (f.w, f.low_res)), Some((128, true)));
    }

    #[test]
    fn read_uses_the_image_chunks_when_they_fit_and_the_file_header_otherwise() {
        let tmp = TmpDir::new("thumbs-dims");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        let store = ThumbStore::new(&root);

        // chunks coherents avec la miniature : repris tels quels (ici ceux d'un fichier plus grand que la source)
        let mut text = foreign_text(&entry);
        text.push(("Thumb::Image::Width", "6000".into()));
        text.push(("Thumb::Image::Height", "4500".into()));
        put_png(&root, "large", &entry, (256, 192), 90, &text);
        assert_eq!(store.read(&entry, 256).map(|f| (f.full_w, f.full_h)), Some((6000, 4500)));

        // chunks aux autres proportions (dimensions avant rotation) : l'en-tete du fichier tranche
        let mut text = foreign_text(&entry);
        text.push(("Thumb::Image::Width", "480".into()));
        text.push(("Thumb::Image::Height", "640".into()));
        put_png(&root, "large", &entry, (256, 192), 90, &text);
        assert_eq!(store.read(&entry, 256).map(|f| (f.full_w, f.full_h)), Some((640, 480)));

        // miniature tournee autrement que le fichier : rejetee
        put_png(&root, "large", &entry, (192, 256), 90, &foreign_text(&entry));
        assert!(store.read(&entry, 256).is_none());
    }

    #[test]
    fn read_ignores_stale_foreign_thumbnails_and_falls_back_to_a_valid_one() {
        let tmp = TmpDir::new("thumbs-foreign-stale");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        seed(&root, &entry, &["normal", "large"]);
        let store = ThumbStore::new(&root);

        // la large est perimee (MTime d'une autre version du fichier) : la normal valide sert
        let mut stale = foreign_text(&entry);
        stale[1].1 = format!("{}.000000", mtime_secs(&entry) - 10);
        put_png(&root, "large", &entry, (256, 192), 90, &stale);
        assert_eq!(store.read(&entry, 256).map(|f| (f.w, f.low_res)), Some((128, true)));

        // Thumb::Size d'une autre version : perimee aussi
        let mut sized = foreign_text(&entry);
        sized.push(("Thumb::Size", (entry.size + 1).to_string()));
        put_png(&root, "large", &entry, (256, 192), 90, &sized);
        assert_eq!(store.read(&entry, 256).map(|f| f.w), Some(128));
        sized.pop();
        sized.push(("Thumb::Size", entry.size.to_string()));
        put_png(&root, "large", &entry, (256, 192), 90, &sized);
        assert_eq!(store.read(&entry, 256).map(|f| f.w), Some(256));

        // sans MTime la miniature ne prouve rien
        put_png(&root, "large", &entry, (256, 192), 90, &foreign_text(&entry)[..1]);
        assert_eq!(store.read(&entry, 256).map(|f| f.w), Some(128));
    }

    #[test]
    fn parse_mtime_accepts_integer_and_fractional_seconds() {
        assert_eq!(parse_mtime("1514745613"), Some(1514745613));
        assert_eq!(parse_mtime("1514745613.000000"), Some(1514745613));
        assert_eq!(parse_mtime(" 1514745613.5 \n"), Some(1514745613));
        assert_eq!(parse_mtime(""), None);
        assert_eq!(parse_mtime("hier"), None);
    }

    #[test]
    fn fail_markers_of_other_programs_are_honoured() {
        let tmp = TmpDir::new("thumbs-fail");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        let store = ThumbStore::new(&root);
        assert!(!store.marked_failed(&entry));

        // pas de dossier d'appli : rien
        std::fs::create_dir_all(root.join("fail")).unwrap();
        assert!(!store.marked_failed(&entry));

        // marqueur d'une autre appli, a jour
        let marker = put_png(&root, "fail/gnome-thumbnail-factory", &entry, (1, 1), 0, &foreign_text(&entry));
        assert!(store.marked_failed(&entry));
        // il ne concerne pas un autre fichier
        let other = Entry { path: entry.path.with_file_name("autre.jpg"), ..entry.clone() };
        assert!(!store.marked_failed(&other));
        // fichier modifie depuis l'echec : on peut reessayer
        let touched = Entry { mtime_ns: entry.mtime_ns + 5_000_000_000, ..entry.clone() };
        assert!(!store.marked_failed(&touched));
        // un marqueur n'est pas une miniature
        assert!(store.read(&entry, 256).is_none());
        // picme n'en ecrit jamais, meme apres avoir ecrit des miniatures
        store.store_standard(&entry, decode(&entry.path, Target::Preview).unwrap()).unwrap();
        assert_eq!(std::fs::read_dir(root.join("fail")).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(marker.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn legacy_directory_is_a_read_only_fallback() {
        let tmp = TmpDir::new("thumbs-legacy");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let (root, legacy) = (tmp.path().join("t"), tmp.path().join(".thumbnails"));
        let store = ThumbStore::new(&root).with_legacy(&legacy);
        assert!(store.read(&entry, 256).is_none());

        // seul l'ancien dossier a quelque chose
        seed(&legacy, &entry, &["large"]);
        assert_eq!(store.read(&entry, 256).map(|f| f.w), Some(256));
        // ses marqueurs d'echec comptent aussi
        assert!(!store.marked_failed(&entry));
        put_png(&legacy, "fail/old-app", &entry, (1, 1), 0, &foreign_text(&entry));
        assert!(store.marked_failed(&entry));

        // la racine prime des qu'elle a une miniature valide, meme plus petite
        seed(&root, &entry, &["normal"]);
        assert_eq!(store.read(&entry, 256).map(|f| f.w), Some(128));

        // jamais d'ecriture dans l'ancien dossier
        let before = walk(&legacy);
        std::fs::remove_dir_all(&root).unwrap();
        store.store_standard(&entry, decode(&entry.path, Target::Preview).unwrap()).unwrap();
        assert_eq!(walk(&legacy), before);
        assert!(root.join("large").is_dir() && root.join("normal").is_dir());
    }

    /// Chemins relatifs de tout ce qui est sous `dir`, tries.
    fn walk(dir: &Path) -> Vec<PathBuf> {
        fn rec(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                out.push(p.strip_prefix(base).unwrap().to_path_buf());
                if p.is_dir() {
                    rec(base, &p, out);
                }
            }
        }
        let mut out = Vec::new();
        rec(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn store_never_overwrites_a_valid_thumbnail_but_replaces_a_stale_one() {
        let tmp = TmpDir::new("thumbs-nooverwrite");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        let store = ThumbStore::new(&root);
        let frame = decode(&entry.path, Target::Fit(256, 256)).unwrap();

        // valide, ecrite par un autre : intacte, et rien ne traine
        let theirs = put_png(&root, "large", &entry, (256, 192), 7, &foreign_text(&entry));
        let bytes = std::fs::read(&theirs).unwrap();
        assert!(!store.store(&entry, &frame, ThumbSize::Large).unwrap());
        assert_eq!(std::fs::read(&theirs).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(root.join("large")).unwrap().count(), 1);

        // perimee : remplacee
        let mut stale = foreign_text(&entry);
        stale[1].1 = "1".into();
        put_png(&root, "large", &entry, (256, 192), 7, &stale);
        assert!(store.store(&entry, &frame, ThumbSize::Large).unwrap());
        let back = store.read(&entry, 256).expect("remplacee par une valide");
        assert_eq!(back.rgba, frame.rgba);
        assert_eq!(std::fs::read_dir(root.join("large")).unwrap().count(), 1);

        // un second appel est sans effet
        let mtime = std::fs::metadata(&theirs).unwrap().modified().unwrap();
        assert!(!store.store(&entry, &frame, ThumbSize::Large).unwrap());
        assert_eq!(std::fs::metadata(&theirs).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn store_standard_writes_large_and_normal_and_skips_tiny_images() {
        let tmp = TmpDir::new("thumbs-standard");
        let entry = copy_fixture(tmp.path(), "a.jpg");
        let root = tmp.path().join("t");
        let store = ThumbStore::new(&root);

        store.store_standard(&entry, decode(&entry.path, Target::Preview).unwrap()).unwrap();
        let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
        dirs.sort();
        assert_eq!(dirs, ["large", "normal"], "ni x-large ni xx-large");
        assert_eq!(store.read(&entry, 128).map(|f| (f.w, f.h, f.low_res)), Some((128, 96, false)));
        assert_eq!(store.read(&entry, 256).map(|f| (f.w, f.h, f.low_res)), Some((256, 192, false)));

        // moins de 128 px : la spec n'en veut pas
        let tiny = Frame { rgba: vec![9; 100 * 80 * 4], w: 100, h: 80, full_w: 100, full_h: 80, low_res: false };
        let tiny_entry = Entry { path: entry.path.with_file_name("tiny.jpg"), ..entry.clone() };
        store.store_standard(&tiny_entry, tiny).unwrap();
        assert!(store.read(&tiny_entry, 256).is_none());
    }
}
