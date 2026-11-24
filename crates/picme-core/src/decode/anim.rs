//! GIF animes : tous les frames avec leur duree, en second temps apres la premiere image (decode ordinaire, `raster`).
//!
//! `image` compose chaque frame sur le canvas entier du GIF (decalage, taille propre, disposal) : les frames rendus
//! ont tous la taille du canvas. Ils restent en memoire sous forme de RGBA : au-dela de `MAX_ANIM_BYTES` cumules ou de
//! `MAX_ANIM_FRAMES` frames, le GIF n'est pas anime (l'appelant garde l'image fixe).

use crate::error::{Error, Result};
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, ImageDecoder};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

/// Plafond des octets RGBA cumules d'une animation : 256 Mo.
pub const MAX_ANIM_BYTES: usize = 256 << 20;

/// Plafond de frames d'une animation.
pub const MAX_ANIM_FRAMES: usize = 2000;

/// Un delai GIF en dessous de ce seuil (0 compris) vaut `DEFAULT_DELAY` : la convention des navigateurs.
const MIN_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_DELAY: Duration = Duration::from_millis(100);

/// Un frame compose sur le canvas entier, affiche pendant `delay`.
#[derive(Debug)]
pub struct AnimFrame {
    pub rgba: Vec<u8>,
    pub delay: Duration,
}

/// Au moins deux frames, tous de `w` x `h` (le canvas du GIF).
#[derive(Debug)]
pub struct Animation {
    pub w: u32,
    pub h: u32,
    pub frames: Vec<AnimFrame>,
}

#[derive(Debug)]
pub enum Outcome {
    Animated(Animation),
    /// Un seul frame : l'image fixe suffit.
    Single,
    /// Plus de `max_bytes` de RGBA ou plus de `MAX_ANIM_FRAMES` frames.
    TooBig,
}

fn frame_delay(delay: image::Delay) -> Duration {
    let d = Duration::from(delay);
    if d < MIN_DELAY { DEFAULT_DELAY } else { d }
}

/// Decode tous les frames du GIF `path`. `cancelled` est interrogee avant chaque frame : vraie, le decode s'arrete
/// (le resultat n'interesse plus personne) sur une `Error::Invalid`. Un fichier tronque ou corrompu est une erreur.
pub fn decode_animation(path: &Path, max_bytes: usize, cancelled: &dyn Fn() -> bool) -> Result<Outcome> {
    let decoder = GifDecoder::new(BufReader::new(File::open(path)?)).map_err(Error::decode)?;
    let (w, h) = decoder.dimensions();
    let frame_bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    if frame_bytes == 0 {
        return Ok(Outcome::Single);
    }
    // avant `into_frames`, qui alloue un canvas entier d'emblee
    if frame_bytes > max_bytes {
        return Ok(Outcome::TooBig);
    }
    let mut frames: Vec<AnimFrame> = Vec::new();
    for frame in decoder.into_frames() {
        if cancelled() {
            return Err(Error::Invalid("decode abandonne".into()));
        }
        let frame = frame.map_err(Error::decode)?;
        if frames.len() >= MAX_ANIM_FRAMES || (frames.len() + 1).saturating_mul(frame_bytes) > max_bytes {
            return Ok(Outcome::TooBig);
        }
        let delay = frame_delay(frame.delay());
        frames.push(AnimFrame { rgba: frame.into_buffer().into_raw(), delay });
    }
    if frames.len() < 2 {
        return Ok(Outcome::Single);
    }
    Ok(Outcome::Animated(Animation { w, h, frames }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};

    const W: u32 = 8;
    const H: u32 = 6;
    const FRAME_BYTES: usize = (W * H * 4) as usize;
    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const GREEN: Rgba<u8> = Rgba([0, 255, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

    /// GIF de W x H, un frame par couple (couleur unie, delai en ms).
    fn write_gif(dir: &TmpDir, name: &str, frames: &[(Rgba<u8>, u32)]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut enc = GifEncoder::new(File::create(&path).unwrap());
        for (color, ms) in frames {
            let img = RgbaImage::from_pixel(W, H, *color);
            enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(*ms, 1))).unwrap();
        }
        drop(enc);
        path
    }

    fn never() -> bool {
        false
    }

    fn animated(out: Outcome) -> Animation {
        match out {
            Outcome::Animated(a) => a,
            other => panic!("animation attendue, pas {other:?}"),
        }
    }

    /// Canal dominant du premier pixel : 0 rouge, 1 vert, 2 bleu.
    fn dominant(rgba: &[u8]) -> usize {
        (0..3).max_by_key(|&c| rgba[c]).unwrap()
    }

    #[test]
    fn frames_come_in_order_at_the_size_of_the_canvas_with_browser_delays() {
        let dir = TmpDir::new("anim-order");
        let path = write_gif(&dir, "a.gif", &[(RED, 0), (GREEN, 50), (BLUE, 200)]);
        let a = animated(decode_animation(&path, MAX_ANIM_BYTES, &never).unwrap());
        assert_eq!((a.w, a.h), (W, H));
        assert_eq!(a.frames.len(), 3);
        // un delai de 0 vaut 100 ms ; les autres sont ceux du fichier (au centieme de seconde)
        let delays: Vec<u64> = a.frames.iter().map(|f| f.delay.as_millis() as u64).collect();
        assert_eq!(delays, [100, 50, 200]);
        assert_eq!(a.frames.iter().map(|f| dominant(&f.rgba)).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(a.frames.iter().all(|f| f.rgba.len() == FRAME_BYTES));
    }

    #[test]
    fn a_delay_under_20_ms_is_100_ms() {
        assert_eq!(frame_delay(Delay::from_numer_denom_ms(0, 1)), Duration::from_millis(100));
        assert_eq!(frame_delay(Delay::from_numer_denom_ms(10, 1)), Duration::from_millis(100));
        assert_eq!(frame_delay(Delay::from_numer_denom_ms(20, 1)), Duration::from_millis(20));
        assert_eq!(frame_delay(Delay::from_numer_denom_ms(40, 1)), Duration::from_millis(40));
    }

    #[test]
    fn a_single_frame_is_not_an_animation() {
        let dir = TmpDir::new("anim-single");
        let path = write_gif(&dir, "one.gif", &[(RED, 100)]);
        assert!(matches!(decode_animation(&path, MAX_ANIM_BYTES, &never).unwrap(), Outcome::Single));
    }

    #[test]
    fn the_memory_cap_keeps_the_still_image() {
        let dir = TmpDir::new("anim-cap");
        let path = write_gif(&dir, "a.gif", &[(RED, 50), (GREEN, 50), (BLUE, 50)]);
        // trois frames tiennent juste, deux seulement non
        assert!(matches!(decode_animation(&path, 3 * FRAME_BYTES, &never).unwrap(), Outcome::Animated(_)));
        assert!(matches!(decode_animation(&path, 3 * FRAME_BYTES - 1, &never).unwrap(), Outcome::TooBig));
        assert!(matches!(decode_animation(&path, 2 * FRAME_BYTES, &never).unwrap(), Outcome::TooBig));
        // le canvas seul depasse le plafond : refuse avant tout decode de frame
        assert!(matches!(decode_animation(&path, FRAME_BYTES - 1, &never).unwrap(), Outcome::TooBig));
    }

    #[test]
    fn a_cancelled_decode_stops_with_an_error() {
        let dir = TmpDir::new("anim-cancel");
        let path = write_gif(&dir, "a.gif", &[(RED, 50), (GREEN, 50)]);
        assert!(matches!(decode_animation(&path, MAX_ANIM_BYTES, &|| true), Err(Error::Invalid(_))));
    }

    #[test]
    fn a_truncated_or_foreign_file_is_an_error() {
        let dir = TmpDir::new("anim-bad");
        let path = write_gif(&dir, "a.gif", &[(RED, 50), (GREEN, 50), (BLUE, 50)]);
        let bytes = std::fs::read(&path).unwrap();
        let cut = dir.path().join("cut.gif");
        std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        assert!(decode_animation(&cut, MAX_ANIM_BYTES, &never).is_err());
        let junk = dir.path().join("junk.gif");
        std::fs::write(&junk, b"pas un gif").unwrap();
        assert!(decode_animation(&junk, MAX_ANIM_BYTES, &never).is_err());
    }

    /// GIF de 2 x 1 fait main : le frame 1 couvre le canvas en rouge (disposal Keep, 50 ms), le frame 2 est un seul
    /// pixel vert en (1, 0), sans delai. Les codes LZW sont ecrits a la main (taille de code minimale 2, donc 3 bits) :
    /// effacement 4, pixels, fin 5.
    #[test]
    fn an_undersized_frame_is_composed_on_the_whole_canvas() {
        let mut gif: Vec<u8> = b"GIF89a".to_vec();
        gif.extend([2, 0, 1, 0, 0x81, 0, 0]);
        gif.extend([0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
        // frame 1 : GCE (Keep, 5 cs), descripteur 0,0 2x1, LZW 4 1 1 5 (rouge, rouge)
        gif.extend([0x21, 0xF9, 4, 0x04, 5, 0, 0, 0]);
        gif.extend([0x2C, 0, 0, 0, 0, 2, 0, 1, 0, 0, 2, 2, 0x4C, 0x0A, 0]);
        // frame 2 : GCE (Keep, 0 cs), descripteur 1,0 1x1, LZW 4 2 5 (vert)
        gif.extend([0x21, 0xF9, 4, 0x04, 0, 0, 0, 0]);
        gif.extend([0x2C, 1, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x54, 0x01, 0]);
        gif.push(0x3B);
        let dir = TmpDir::new("anim-sub");
        let path = dir.path().join("sub.gif");
        std::fs::write(&path, &gif).unwrap();

        let a = animated(decode_animation(&path, MAX_ANIM_BYTES, &never).unwrap());
        assert_eq!((a.w, a.h), (2, 1));
        assert!(a.frames.iter().all(|f| f.rgba.len() == 2 * 4));
        assert_eq!(a.frames[0].rgba, [255, 0, 0, 255, 255, 0, 0, 255]);
        // le pixel hors du frame 2 garde le rouge du frame 1
        assert_eq!(a.frames[1].rgba, [255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!([a.frames[0].delay.as_millis(), a.frames[1].delay.as_millis()], [50, 100]);
    }
}
