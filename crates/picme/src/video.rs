//! Video par GStreamer : la premiere image (miniatures du filmstrip et du cache freedesktop, sans fenetre) et la
//! lecture (`playbin` vers `gtk4paintablesink`, dont le `Paintable` s'affiche par-dessus le canvas). Le son est
//! coupe par defaut : sans le drapeau audio de `playbin`, aucun decodeur ni peripherique audio n'est ouvert ; M
//! recree le lecteur a la meme position avec le son.

use gstgtk4 as _;
use gstreamer as gst;
use gst::prelude::*;
use gtk4::gdk;
use picme_core::decode::Frame;
use picme_core::Error;
use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Once, OnceLock};

/// Attente du preroll d'une miniature : une video illisible ne bloque pas un thread du pool.
const PREROLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);
/// Saut de Maj+fleche, en secondes.
pub const SEEK_STEP: i64 = 5;

fn init() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| gst::init().map_err(|e| e.to_string())).clone()
}

/// Remet le pipeline a `Null` quoi qu'il arrive (retour anticipe, panique).
struct Stop(gst::Element);

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn uri_of(path: &Path) -> Result<String, String> {
    gst::glib::filename_to_uri(path, None).map(|u| u.to_string()).map_err(|e| e.to_string())
}

/// Attend la fin d'un changement d'etat asynchrone (preroll ou seek). `Err` si le pipeline signale une erreur.
fn wait_async_done(bus: &gst::Bus, timeout: gst::ClockTime) -> Result<bool, String> {
    use gst::MessageView;
    let Some(msg) = bus.timed_pop_filtered(timeout, &[gst::MessageType::AsyncDone, gst::MessageType::Error]) else { return Ok(false) };
    match msg.view() {
        MessageView::Error(e) => Err(e.error().to_string()),
        _ => Ok(true),
    }
}

/// Premiere image d'une video, en RGBA, a sa taille native et orientee (rotation des telephones appliquee par le
/// sink de GStreamer ; ici `videoflip` la lit dans les tags). Lue un peu apres le debut (un dixieme de la duree, au
/// plus 10 s) : la toute premiere image est souvent noire. Bloquante, pour un thread du pool.
pub fn first_frame(path: &Path) -> Result<Frame, Error> {
    let fail = |m: String| Error::Unsupported(format!("video {} : {m}", path.display()));
    init().map_err(&fail)?;
    let sink_bin = gst::parse::bin_from_description(
        "videoflip video-direction=auto ! videoconvert ! capsfilter caps=video/x-raw,format=RGBA ! fakesink name=thumb sync=false enable-last-sample=true",
        true,
    )
    .map_err(|e| fail(e.to_string()))?;
    let thumb = sink_bin.by_name("thumb").ok_or_else(|| fail("fakesink absent".into()))?;
    let pipeline = gst::ElementFactory::make("playbin").property("uri", uri_of(path).map_err(&fail)?).build().map_err(|e| fail(e.to_string()))?;
    pipeline.set_property("video-sink", &sink_bin);
    // ni son ni sous-titres
    pipeline.set_property_from_str("flags", "video");
    let _stop = Stop(pipeline.clone());
    let bus = pipeline.bus().ok_or_else(|| fail("pas de bus".into()))?;

    pipeline.set_state(gst::State::Paused).map_err(|e| fail(e.to_string()))?;
    if !wait_async_done(&bus, PREROLL_TIMEOUT).map_err(&fail)? {
        return Err(fail("delai depasse".into()));
    }
    if let Some(duration) = pipeline.query_duration::<gst::ClockTime>().filter(|d| d.nseconds() > 0) {
        let at = (duration / 10).min(gst::ClockTime::from_seconds(10));
        // un seek raté ne coute que la qualite de l'image : le preroll de la premiere image reste
        if pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, at).is_ok() {
            let _ = wait_async_done(&bus, PREROLL_TIMEOUT);
        }
    }
    let sample = thumb.property::<Option<gst::Sample>>("last-sample").ok_or_else(|| fail("pas d'image".into()))?;
    frame_of(&sample).ok_or_else(|| fail("image illisible".into()))
}

/// RGBA serre (largeur x 4 octets par ligne, sans marge) d'un sample du `capsfilter` RGBA.
fn frame_of(sample: &gst::Sample) -> Option<Frame> {
    let caps = sample.caps()?;
    let s = caps.structure(0)?;
    let (w, h) = (s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?);
    let (w, h) = (u32::try_from(w).ok().filter(|&w| w > 0)?, u32::try_from(h).ok().filter(|&h| h > 0)?);
    let map = sample.buffer()?.map_readable().ok()?;
    let need = w as usize * h as usize * 4;
    (map.len() >= need).then(|| Frame { rgba: map[..need].to_vec(), w, h, full_w: w, full_h: h, low_res: false, partial: false })
}

/// Ce que le lecteur apprend au main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Fin du media : le lecteur reste sur la derniere image.
    Eos,
    Error(String),
}

/// Lecteur d'un fichier video, sur le main thread. Detruit (etat `Null`) a sa chute : le fichier est relache avant
/// la corbeille, un Move ou la fermeture.
pub struct Player {
    pipeline: gst::Element,
    paintable: gdk::Paintable,
    audio: bool,
    paused: Cell<bool>,
    ended: Cell<bool>,
    _watch: gst::bus::BusWatchGuard,
}

impl Player {
    /// Ouvre `path` a `start`, en lecture ou en pause. `audio` : le son est decode et joue.
    pub fn open(path: &Path, audio: bool, start: gst::ClockTime, paused: bool, on_event: impl Fn(Event) + 'static) -> Result<Player, String> {
        init()?;
        static SINK: Once = Once::new();
        let mut registered = Ok(());
        SINK.call_once(|| registered = gstgtk4::plugin_register_static().map_err(|e| e.to_string()));
        registered?;
        let sink = gst::ElementFactory::make("gtk4paintablesink").build().map_err(|e| e.to_string())?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        let pipeline = gst::ElementFactory::make("playbin").property("uri", uri_of(path)?).build().map_err(|e| e.to_string())?;
        pipeline.set_property("video-sink", &sink);
        pipeline.set_property_from_str("flags", if audio { "video+audio" } else { "video" });
        let bus = pipeline.bus().ok_or("pas de bus")?;
        let on_event = Rc::new(on_event);
        let watch = bus
            .add_watch_local(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Eos(_) => on_event(Event::Eos),
                    gst::MessageView::Error(e) => on_event(Event::Error(e.error().to_string())),
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            })
            .map_err(|e| e.to_string())?;
        let player = Player { pipeline, paintable, audio, paused: Cell::new(paused), ended: Cell::new(false), _watch: watch };
        // le seek de depart se fait au preroll : playbin n'accepte pas de seek en `Ready`
        player.pipeline.set_state(gst::State::Paused).map_err(|e| e.to_string())?;
        if start > gst::ClockTime::ZERO {
            let _ = player.pipeline.state(PREROLL_TIMEOUT);
            let _ = player.pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, start);
        }
        if !paused {
            player.pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        }
        Ok(player)
    }

    pub fn paintable(&self) -> &gdk::Paintable {
        &self.paintable
    }

    pub fn audio(&self) -> bool {
        self.audio
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Espace : pause ou reprise ; a la fin du media, relance depuis le debut. Rend vrai quand le lecteur est en pause.
    pub fn toggle_pause(&self) -> bool {
        if self.ended.replace(false) {
            let _ = self.pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, gst::ClockTime::ZERO);
            let _ = self.pipeline.set_state(gst::State::Playing);
            self.paused.set(false);
            return false;
        }
        let pause = !self.paused.get();
        if self.pipeline.set_state(if pause { gst::State::Paused } else { gst::State::Playing }).is_ok() {
            self.paused.set(pause);
        }
        self.paused.get()
    }

    /// Le media est fini : la derniere image reste, en pause.
    pub fn finish(&self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
        self.paused.set(true);
        self.ended.set(true);
    }

    pub fn position(&self) -> gst::ClockTime {
        self.pipeline.query_position::<gst::ClockTime>().unwrap_or(gst::ClockTime::ZERO)
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.pipeline.query_duration::<gst::ClockTime>()
    }

    /// Maj+fleches : saute de `secs` secondes (negatif : en arriere), borne au media.
    pub fn seek_by(&self, secs: i64) {
        let target = target_of(self.position().nseconds(), secs, self.duration().map(|d| d.nseconds()));
        let _ = self.pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, gst::ClockTime::from_nseconds(target));
        self.ended.set(false);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Position visee par un saut de `secs` depuis `pos` (ns), bornee a [0, duree[ ; un saut qui depasserait la fin tombe
/// juste avant (une image encore visible plutot qu'un EOS immediat).
pub fn target_of(pos_ns: u64, secs: i64, duration_ns: Option<u64>) -> u64 {
    let moved = pos_ns.saturating_add_signed(secs.saturating_mul(1_000_000_000));
    match duration_ns {
        Some(d) => moved.min(d.saturating_sub(100_000_000)),
        None => moved,
    }
}

/// "m:ss" d'une position, pour l'OSD.
pub fn clock(t: gst::ClockTime) -> String {
    let s = t.seconds();
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use picme_core::fixtures_dir;

    #[test]
    fn seek_targets_stay_inside_the_media() {
        let s = 1_000_000_000u64;
        assert_eq!(target_of(10 * s, 5, Some(60 * s)), 15 * s);
        assert_eq!(target_of(2 * s, -5, Some(60 * s)), 0);
        assert_eq!(target_of(58 * s, 5, Some(60 * s)), 60 * s - 100_000_000);
        assert_eq!(target_of(10 * s, 5, None), 15 * s);
    }

    #[test]
    fn clock_is_minutes_and_two_digit_seconds() {
        assert_eq!(clock(gst::ClockTime::from_seconds(75)), "1:15");
        assert_eq!(clock(gst::ClockTime::ZERO), "0:00");
    }

    #[test]
    fn the_first_frame_of_the_fixture_is_a_real_picture() {
        let f = first_frame(&fixtures_dir().join("j.mov")).unwrap();
        assert!(f.w > 0 && f.h > 0 && f.rgba.len() == f.w as usize * f.h as usize * 4);
        let px = f.rgba.as_chunks::<4>().0;
        assert!(px.iter().all(|p| p[3] == 255), "opaque");
        // videotestsrc : des barres de couleur, pas une image unie
        let distinct = px.iter().take(f.w as usize).map(|p| [p[0], p[1], p[2]]).collect::<std::collections::HashSet<_>>().len();
        assert!(distinct > 3, "{distinct} couleurs sur la premiere ligne");
    }

    #[test]
    fn a_file_that_is_not_a_video_fails_without_hanging() {
        let tmp = crate::testutil::TmpDir::new("video-garbage");
        let p = tmp.path().join("x.mov");
        std::fs::write(&p, b"pas une video").unwrap();
        let t = std::time::Instant::now();
        assert!(matches!(first_frame(&p), Err(Error::Unsupported(_))));
        assert!(t.elapsed() < std::time::Duration::from_secs(12));
    }
}
