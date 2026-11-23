//! Outil de banc, pas un test : fabrique un cache de miniatures comme celui d'un file manager (`normal` 128 px et
//! `xx-large` 1024 px, `Thumb::MTime` fractionnaire, ni `Thumb::Size` ni `Thumb::Image::*`) pour les images d'un
//! dossier. Lance a la main :
//!
//! `PICME_SEED_SRC=/work/target/bench/nav PICME_SEED_OUT=/work/target/bench/cache-seed-xxl/thumbnails \
//!  cargo test -p picme-core --release --test seed_foreign_cache -- --ignored`
//!
//! `PICME_SEED_URI_DIR` (defaut : le dossier source) est le dossier qu'`picme` verra au moment du banc, quand il
//! differe de celui d'ici.

use picme_core::decode::{Target, decode};
use picme_core::thumbs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn write_png(out: &Path, uri: &str, secs: u64, w: u32, h: u32, rgba: &[u8]) {
    let name = {
        use md5::{Digest, Md5};
        Md5::digest(uri.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    std::fs::create_dir_all(out).unwrap();
    let mut file = std::fs::File::create(out.join(format!("{name}.png"))).unwrap();
    let mut enc = png::Encoder::new(&mut file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.add_text_chunk("Thumb::URI".into(), uri.into()).unwrap();
    enc.add_text_chunk("Thumb::MTime".into(), format!("{secs}.000000")).unwrap();
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
    writer.finish().unwrap();
    file.flush().unwrap();
}

#[test]
#[ignore = "outil de banc : PICME_SEED_SRC et PICME_SEED_OUT"]
fn seed_foreign_cache() {
    let src = PathBuf::from(std::env::var_os("PICME_SEED_SRC").expect("PICME_SEED_SRC"));
    let out = PathBuf::from(std::env::var_os("PICME_SEED_OUT").expect("PICME_SEED_OUT"));
    let uri_dir = std::env::var_os("PICME_SEED_URI_DIR").map(PathBuf::from).unwrap_or_else(|| src.clone());
    let mut n = 0;
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "jpg") {
            continue;
        }
        let secs = e.metadata().unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let uri = thumbs::uri(&uri_dir.join(path.file_name().unwrap()));
        let big = decode(&path, Target::Fit(1024, 1024)).unwrap();
        write_png(&out.join("xx-large"), &uri, secs, big.w, big.h, &big.rgba);
        let small = big.shrink_to(128, 128).unwrap();
        write_png(&out.join("normal"), &uri, secs, small.w, small.h, &small.rgba);
        n += 1;
    }
    eprintln!("seed_foreign_cache : {n} images -> {}", out.display());
}
