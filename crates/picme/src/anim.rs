//! GIF animes, cote interface : le job de decode qui s'arrete des que son resultat n'interesse plus, et l'ordre de
//! lecture des frames (boucle infinie, pause). Le decode est dans `picme_core::decode::anim` ; les timers et le
//! canvas sont dans `main`.

use picme_core::decode::{self, AnimOutcome, MAX_ANIM_BYTES};
use picme_core::error::Result;
use picme_core::pool::Key;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Le GIF dont l'animation est voulue : un seul a la fois, celui du courant. Le travail le consulte avant chaque
/// frame et s'arrete (`Error::Invalid`) des qu'un autre fichier est voulu, ou plus aucun ; le pool, lui, ne sait pas
/// annuler un job lance.
#[derive(Clone, Default)]
pub struct Wanted(Arc<Mutex<Option<Key>>>);

impl Wanted {
    pub fn set(&self, key: Option<Key>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = key;
    }

    pub fn is(&self, key: &Key) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(key)
    }
}

/// Le travail du pool `anim` : tous les frames du GIF de la cle, tant que la cle est voulue.
pub fn work(wanted: Wanted) -> impl Fn(&Key) -> Result<AnimOutcome> + Send + Sync + 'static {
    move |key: &Key| decode::decode_animation(&key.path, MAX_ANIM_BYTES, &|| !wanted.is(key))
}

/// Les frames d'une animation et celui qui est a l'ecran. Au moins un frame.
pub struct Playback<T> {
    frames: Vec<(T, Duration)>,
    at: usize,
    paused: bool,
}

impl<T> Playback<T> {
    pub fn new(frames: Vec<(T, Duration)>) -> Self {
        assert!(!frames.is_empty(), "une animation a des frames");
        Playback { frames, at: 0, paused: false }
    }

    pub fn frame(&self) -> &T {
        &self.frames[self.at].0
    }

    /// Duree d'affichage du frame courant.
    pub fn delay(&self) -> Duration {
        self.frames[self.at].1
    }

    /// Frame suivant, le premier apres le dernier (boucle infinie).
    pub fn advance(&mut self) -> &T {
        self.at = (self.at + 1) % self.frames.len();
        self.frame()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Pause ou reprise ; rend le nouvel etat (vrai : en pause).
    pub fn toggle_pause(&mut self) -> bool {
        self.paused = !self.paused;
        self.paused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(name: &str) -> Key {
        Key { path: PathBuf::from(name), mtime_ns: 1, size: 2, target: decode::Target::Full }
    }

    #[test]
    fn wanted_is_the_one_key_set_last() {
        let w = Wanted::default();
        assert!(!w.is(&key("a.gif")), "rien de voulu au depart");
        w.set(Some(key("a.gif")));
        assert!(w.is(&key("a.gif")));
        assert!(!w.is(&key("b.gif")));
        // une autre version du meme fichier n'est pas la voulue
        assert!(!w.is(&Key { mtime_ns: 9, ..key("a.gif") }));
        let shared = w.clone();
        shared.set(None);
        assert!(!w.is(&key("a.gif")), "les clones partagent l'etat");
    }

    /// GIF de 2 x 1 a deux frames (rouge, puis un pixel vert), ecrit a la main.
    const GIF: &[u8] = &[
        b'G', b'I', b'F', b'8', b'9', b'a', 2, 0, 1, 0, 0x81, 0, 0, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0x21, 0xF9, 4, 0x04, 5, 0, 0, 0, 0x2C, 0, 0,
        0, 0, 2, 0, 1, 0, 0, 2, 2, 0x4C, 0x0A, 0, 0x21, 0xF9, 4, 0x04, 5, 0, 0, 0, 0x2C, 1, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x54, 0x01, 0, 0x3B,
    ];

    #[test]
    fn work_decodes_a_wanted_key_and_stops_for_one_that_is_not() {
        let dir = crate::testutil::TmpDir::new("anim-work");
        let path = dir.path().join("a.gif");
        std::fs::write(&path, GIF).unwrap();
        let k = Key { path, mtime_ns: 1, size: GIF.len() as u64, target: decode::Target::Full };

        let wanted = Wanted::default();
        let work = work(wanted.clone());
        // rien de voulu, ou un autre fichier : le job abandonne avant le premier frame
        assert!(matches!(work(&k), Err(picme_core::Error::Invalid(_))));
        wanted.set(Some(key("b.gif")));
        assert!(matches!(work(&k), Err(picme_core::Error::Invalid(_))));
        // voulu : les deux frames
        wanted.set(Some(k.clone()));
        match work(&k) {
            Ok(AnimOutcome::Animated(a)) => assert_eq!((a.w, a.h, a.frames.len()), (2, 1, 2)),
            other => panic!("animation attendue, pas {other:?}"),
        }
    }

    #[test]
    fn frames_play_in_order_and_loop_forever() {
        let ms = Duration::from_millis;
        let mut p = Playback::new(vec![("a", ms(100)), ("b", ms(50)), ("c", ms(200))]);
        assert_eq!((*p.frame(), p.delay()), ("a", ms(100)));
        assert_eq!(*p.advance(), "b");
        assert_eq!(p.delay(), ms(50));
        assert_eq!(*p.advance(), "c");
        assert_eq!(p.delay(), ms(200));
        assert_eq!(*p.advance(), "a", "apres le dernier, le premier");
        for _ in 0..30 {
            p.advance();
        }
        assert_eq!(*p.frame(), "a", "10 tours de plus, au meme point");
    }

    #[test]
    fn pause_keeps_the_frame_and_toggles() {
        let mut p = Playback::new(vec![(1, Duration::from_millis(10)), (2, Duration::from_millis(10))]);
        assert!(!p.paused());
        p.advance();
        assert!(p.toggle_pause());
        assert!(p.paused());
        assert_eq!(*p.frame(), 2);
        assert!(!p.toggle_pause());
    }
}
