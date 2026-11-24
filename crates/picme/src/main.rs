use gtk4 as gtk;
use gtk::{gdk, glib, prelude::*};
use picme::anim::{self, Playback};
use picme::app::{self, Controller, Level, Show};
use picme::canvas::{self, Canvas};
use gstreamer as gst;
use picme::chrome::Chrome;
use picme::crop::Cropper;
use picme::dnd;
use picme::fileops::{Done, FileOps, FileOpsHandle};
use picme::filmstrip::Filmstrip;
use picme::help::Help;
use picme::info::InfoPanel;
use picme::keys::{self, Action, Ctx};
use picme::launch::{self, Cli};
use picme::monitor::DirWatch;
use picme::osd::{self, Osd};
use picme::video::{self, Player};
use picme::view::ZOOM_STEP;
use picme_core::decode::{AnimOutcome, Animation, DEFAULT_BUDGET_BYTES, Frame, Target};
use picme_core::dir::{self, Entry};
use picme_core::map;
use picme_core::media::MediaKind;
use picme_core::meta;
use picme_core::orient::Orientation;
use picme_core::pool::{self, Key, Pool};
use picme_core::serve;
use picme_core::thumbs::ThumbStore;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CSS: &str = "
window { background: #000; }
.osd { background: rgba(0, 0, 0, 0.6); color: #eee; padding: 4px 10px; border-radius: 6px; font-size: 13px; }
.help { padding: 18px 26px; font-size: 14px; background: rgba(0, 0, 0, 0.82); border-radius: 10px; }
.live-badge { background: rgba(0, 0, 0, 0.6); color: #eee; padding: 1px 8px; border-radius: 10px; font-size: 11px; font-weight: bold; }
.strip-bar { background: rgba(0, 0, 0, 0.7); }
.strip { background: transparent; }
.strip row { padding: 0; margin: 0; min-height: 0; background: transparent; }
.strip row:selected { background: transparent; outline: 2px solid #fff; outline-offset: -2px; }
.strip-cell { background: rgba(255, 255, 255, 0.08); }
";

/// `PICME_TRACE=1` : horodatages sur stderr, en ms depuis le debut de `main`.
#[derive(Clone, Copy)]
struct Trace {
    on: bool,
    t0: Instant,
}

impl Trace {
    fn mark(&self, what: &str) {
        if self.on {
            eprintln!("[picme {:7.1} ms] {what}", self.t0.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

/// `PICME_BENCH_NAV=N[:ms]` : N fois image suivante toutes les `ms` (40 par defaut, le rythme d'une touche
/// maintenue) des que la premiere image est peinte, puis resume sur stderr et sortie.
struct Bench {
    left: u32,
    every: Duration,
    /// Navigation en attente de sa premiere image ; puis de son premier frame peint avec cette image.
    waiting: Option<Instant>,
    painting: Option<(Instant, Level)>,
    /// Latence navigation -> frame peint avec la nouvelle image, et niveau de cette image.
    done: Vec<(Duration, Level)>,
    /// Navigations jamais affichees (la suivante est arrivee avant leur image).
    skipped: u32,
    /// Retard maximal d'un tick sur son echeance : le temps ou le main thread n'a pas repondu.
    late: Duration,
    rss_max_kb: u64,
    /// VmRSS toutes les 100 navigations, pour voir le plateau.
    rss_every_100: Vec<u64>,
}

/// Le GIF anime en lecture : ses frames (le premier a l'ecran au depart) et, pour l'arret, l'image fixe que le canvas
/// montrait avant (mise a jour par chaque `Fit` ou `Full` que le controller rend du meme fichier).
struct Anim {
    key: Key,
    play: Playback<gdk::Texture>,
    /// Dimensions de l'image entiere.
    w: u32,
    h: u32,
    still: Option<(gdk::Texture, u32, u32)>,
}

fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok()).unwrap_or(0)
}

struct Ui {
    window: gtk::Window,
    canvas: Canvas,
    ctl: RefCell<Controller<gdk::Texture>>,
    trace: Trace,
    bench: RefCell<Option<Bench>>,
    painted_image: Cell<bool>,
    /// Fin de repetition programmee (`Controller::settle`).
    settle: RefCell<Option<glib::SourceId>>,
    osd: Rc<Osd>,
    help: Help,
    /// Panneau des metadonnees (I) et le fichier dont la lecture, sur un autre thread, est attendue.
    info: InfoPanel,
    info_want: RefCell<Option<PathBuf>>,
    info_tx: async_channel::Sender<(PathBuf, meta::Info)>,
    /// La page de la carte (G) est en preparation sur un autre thread : un second G n'en lance pas une autre.
    map_busy: Cell<bool>,
    map_tx: async_channel::Sender<MapResult>,
    /// Le serveur qui sert la carte au navigateur : il s'arrete seul 10 minutes apres la derniere consultation, au
    /// prochain G ou a la sortie de picme.
    map_server: RefCell<Option<serve::Server>>,
    strip: Rc<Filmstrip>,
    chrome: Rc<Chrome>,
    /// Le courant a la derniere mise a jour : un autre fichier remet la vue au fit.
    last_current: RefCell<Option<PathBuf>>,
    watch: RefCell<Option<DirWatch>>,
    fileops: FileOpsHandle,
    /// Quarts de tour donnes a la vue depuis sa remise a zero pour suivre `Controller::view_quarters`.
    turned: Cell<i32>,
    /// Keycode de la touche Suppr enfoncee : sa repetition ne vide pas le dossier.
    delete_held: Cell<Option<u32>>,
    /// Cadre de recadrage (C), par-dessus le canvas.
    cropper: Rc<Cropper>,
    /// Lecteur en cours : le fichier qu'il lit (la video courante, ou le `.mov` d'une Live Photo tenue par L).
    player: RefCell<Option<(PathBuf, Player)>>,
    /// Image de la video, par-dessus le canvas qui montre en dessous la miniature de la video.
    video_view: gtk::Picture,
    /// Son voulu (M) : coupe par defaut, et coupe de nouveau si la sortie audio manque.
    audio: Cell<bool>,
    /// L est tenue : le `.mov` de la Live Photo joue.
    live_held: Cell<bool>,
    live_badge: gtk::Label,
    /// Le pool qui decode les frames d'un GIF anime, et le GIF voulu : le job en cours s'arrete quand il change.
    anim_pool: Pool<Key>,
    anim_want: anim::Wanted,
    /// Le dernier GIF dont le decode a ete demande : un GIF n'est demande qu'une fois, sauf decode abandonne.
    anim_tried: RefCell<Option<Key>>,
    /// L'animation en lecture, et le timer de son prochain frame (jamais un id deja servi).
    anim: RefCell<Option<Anim>>,
    anim_tick: RefCell<Option<glib::SourceId>>,
    /// Les scans de dossier (au demarrage, puis a chaque fichier depose d'un autre dossier).
    scan_tx: async_channel::Sender<ScanResult>,
}

type ScanResult = (PathBuf, std::io::Result<Vec<Entry>>);

/// La page HTML de la carte du dossier ; `None` : aucune photo geolocalisee.
type MapResult = Option<String>;

/// Liste `dir` sur un autre thread : la fenetre ne l'attend pas.
fn spawn_scan(dir: PathBuf, recursive: bool, tx: async_channel::Sender<ScanResult>) {
    std::thread::Builder::new()
        .name("picme-scan".into())
        .spawn(move || {
            let list = if recursive { dir::list_tree(&dir) } else { dir::list(&dir) };
            let _ = tx.send_blocking((dir, list));
        })
        .expect("thread scan");
}

impl Ui {
    /// O : le dossier courant dans le gestionnaire de fichiers du systeme (handler `inode/directory`).
    fn open_folder(&self) {
        let ctl = self.ctl.borrow();
        // en mode `-r` le dossier de la liste est la racine : on ouvre celui de l'image affichee
        let dir = ctl.current().and_then(|e| e.path.parent().map(Path::to_path_buf)).or_else(|| ctl.dir().map(Path::to_path_buf));
        drop(ctl);
        let Some(dir) = dir else { return };
        let uri = gtk4::gio::File::for_path(&dir).uri();
        match gtk4::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>) {
            Ok(()) => self.osd.message("dossier ouvert"),
            Err(e) => self.osd.message(&format!("explorateur indisponible : {e}")),
        }
    }

    /// I : les metadonnees du courant sont lues sur un autre thread, le panneau s'ouvre quand elles arrivent (voir
    /// `on_info`).
    fn show_info(&self) {
        let Some(path) = self.ctl.borrow().current().map(|e| e.path.clone()) else { return };
        *self.info_want.borrow_mut() = Some(path.clone());
        let tx = self.info_tx.clone();
        std::thread::Builder::new()
            .name("picme-info".into())
            .spawn(move || {
                let info = meta::read(&path);
                let _ = tx.send_blocking((path, info));
            })
            .expect("thread info");
    }

    /// Les metadonnees de `path` sont lues : le panneau s'ouvre si c'est toujours ce que l'utilisateur attend (une
    /// autre image, ou une autre touche, a pu passer entre-temps).
    fn on_info(&self, path: &Path, info: &meta::Info) {
        let wanted = self.info_want.borrow_mut().take();
        let still_current = self.ctl.borrow().current().is_some_and(|e| e.path == path);
        if wanted.as_deref() == Some(path) && still_current {
            self.info.show(info);
        }
    }

    /// G : lit la position des photos de la liste courante (tout l'arbre en mode `-r`) et ecrit la page de la carte
    /// sur un autre thread, puis la sert en local et l'ouvre dans le navigateur (voir `on_map`).
    fn open_map(&self) {
        if self.map_busy.get() {
            self.osd.message("carte en préparation");
            return;
        }
        let (entries, current, dir) = {
            let ctl = self.ctl.borrow();
            let current = ctl.current().map(|e| e.path.clone());
            let dir = ctl.dir().map(Path::to_path_buf).or_else(|| current.as_ref().and_then(|p| p.parent().map(Path::to_path_buf)));
            (ctl.entries().to_vec(), current, dir)
        };
        let Some(dir) = dir else { return };
        self.map_busy.set(true);
        self.osd.message("carte en préparation");
        let tx = self.map_tx.clone();
        std::thread::Builder::new()
            .name("picme-map".into())
            .spawn(move || {
                let _ = tx.send_blocking(map::make(&entries, current.as_deref(), &dir));
            })
            .expect("thread carte");
    }

    /// La page de la carte est prete : un serveur sur `127.0.0.1` la sert (une page `file://` n'envoie pas de Referer et
    /// les tuiles OpenStreetMap la refusent) et le navigateur par defaut ouvre son adresse. Un G de plus remplace le
    /// serveur : l'ancien s'arrete, son onglet ne se recharge plus.
    fn on_map(&self, result: MapResult) {
        self.map_busy.set(false);
        let Some(page) = result else {
            self.osd.message("aucune photo géolocalisée dans ce dossier");
            return;
        };
        let server = match serve::Server::start(page) {
            Ok(s) => s,
            Err(e) => {
                self.osd.message(&format!("carte impossible : serveur local sur 127.0.0.1 : {e}"));
                return;
            }
        };
        match gtk4::gio::AppInfo::launch_default_for_uri(&server.url(), None::<&gtk4::gio::AppLaunchContext>) {
            Ok(()) => {
                self.osd.message("carte ouverte dans le navigateur");
                // l'ancien serveur est lache ici, donc arrete
                *self.map_server.borrow_mut() = Some(server);
            }
            Err(e) => self.osd.message(&format!("navigateur indisponible : {e}")),
        }
    }

    fn apply(self: &Rc<Self>, show: Show<gdk::Texture>) {
        let show = self.filter_anim(show);
        match show {
            Show::Keep => {}
            Show::Image(img) => {
                self.canvas.set_image(&img.texture, img.full_w, img.full_h);
                self.announce();
                if let Some(b) = self.bench.borrow_mut().as_mut() {
                    if let Some(t) = b.waiting.take() {
                        b.painting = Some((t, img.level));
                    }
                } else if self.trace.on {
                    let name = self.ctl.borrow().current().map(|e| e.path.display().to_string()).unwrap_or_default();
                    self.trace.mark(&format!("show {:?} {}x{} {name}", img.level, img.w, img.h));
                }
            }
            Show::Clear => self.canvas.clear(),
            Show::Failed(msg) => {
                eprintln!("picme : {msg}");
                self.osd.message(&msg);
                self.canvas.clear();
            }
        }
        let current = self.ctl.borrow().current().map(|e| e.path.clone());
        if current.is_some() {
            self.osd.unpin();
        }
        if let Some(path) = &current {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if self.window.title().as_deref() != Some(name.as_str()) {
                self.window.set_title(Some(&name));
            }
        }
        if *self.last_current.borrow() != current {
            *self.last_current.borrow_mut() = current;
            // un autre fichier : ni cadre de crop, ni lecture, ni Live Photo tenue, ni panneau d'un autre fichier
            self.info.hide();
            self.info_want.borrow_mut().take();
            self.cropper.stop();
            self.live_held.set(false);
            self.stop_player();
            self.reset_view();
            self.chrome.pulse();
            self.announce();
        }
        self.live_badge.set_visible(self.ctl.borrow().current().is_some_and(|e| e.live_mov.is_some()));
        self.sync();
        self.sync_turns();
        self.sync_player();
        self.sync_anim();
    }

    /// Une animation en lecture garde l'ecran : l'image fixe du meme fichier que rend le controller (`Fit`, `Full`) est
    /// gardee pour l'arret, sans toucher au canvas ; tout autre changement (un autre fichier, une autre version du
    /// fichier, plus d'image) arrete l'animation et laisse l'image fixe a l'ecran.
    fn filter_anim(&self, show: Show<gdk::Texture>) -> Show<gdk::Texture> {
        let Some(playing) = self.anim.borrow().as_ref().map(|a| a.key.clone()) else { return show };
        let current = self.ctl.borrow().current().map(|e| Key::of(e, Target::Full));
        if current.as_ref() != Some(&playing) {
            self.stop_anim(true);
            return show;
        }
        match show {
            Show::Image(img) => {
                if let Some(a) = self.anim.borrow_mut().as_mut() {
                    a.still = Some((img.texture, img.full_w, img.full_h));
                }
                Show::Keep
            }
            Show::Keep => Show::Keep,
            other => {
                self.stop_anim(false);
                other
            }
        }
    }

    /// Le courant est un GIF dont une image est a l'ecran : ses frames se decodent en second temps (voir `on_anim`),
    /// jamais pendant la repetition d'une touche ni sous un cadre de crop. Le job d'un autre GIF s'arrete ou quitte la file.
    fn sync_anim(&self) {
        let key = {
            let ctl = self.ctl.borrow();
            ctl.current_shown()
                .filter(|e| e.kind == MediaKind::Gif && !ctl.repeating() && !self.cropper.is_active())
                .map(|e| Key::of(e, Target::Full))
        };
        self.anim_want.set(key.clone());
        self.anim_pool.retain(|k| Some(k) == key.as_ref());
        // `anim_tried` ne vaut que le temps de l'affichage : un autre fichier a l'ecran (ou aucun) l'efface, pour que
        // le retour sur le GIF relance son animation
        if self.anim_tried.borrow().as_ref() != key.as_ref() {
            self.anim_tried.borrow_mut().take();
        }
        let Some(key) = key else { return };
        if self.anim.borrow().is_some() || self.anim_tried.borrow().as_ref() == Some(&key) {
            return;
        }
        *self.anim_tried.borrow_mut() = Some(key.clone());
        self.anim_pool.submit(key, 0);
    }

    /// Les frames d'un GIF sont decodes. Une erreur de decode laisse l'image fixe, sans bruit ; un seul frame aussi ; un
    /// GIF au-dela du plafond de memoire le dit en une ligne.
    fn on_anim(self: &Rc<Self>, key: Key, result: picme_core::Result<AnimOutcome>) {
        let wanted = self.anim_want.is(&key);
        match result {
            // decode abandonne parce qu'un autre fichier etait voulu : si le fichier est de nouveau voulu (aller-retour),
            // il repart ; sinon il sera redemande a son prochain affichage
            Err(picme_core::Error::Invalid(_)) => {
                if wanted {
                    self.anim_pool.submit(key, 0);
                } else if self.anim_tried.borrow().as_ref() == Some(&key) {
                    self.anim_tried.borrow_mut().take();
                }
            }
            Err(_) => {}
            Ok(_) if !wanted => {}
            Ok(AnimOutcome::Single) => {}
            Ok(AnimOutcome::TooBig) => self.osd.message("animation trop lourde (plus de 256 Mo décodée) : image fixe"),
            Ok(AnimOutcome::Animated(a)) => self.play_anim(key, a),
        }
    }

    /// Le canvas montre les frames, l'un apres l'autre, au rythme du fichier ; zoom, deplacement et rotation restent
    /// ceux de l'image.
    fn play_anim(self: &Rc<Self>, key: Key, a: Animation) {
        let (w, h) = (a.w, a.h);
        let frames = a.frames.into_iter().map(|f| (canvas::rgba_texture(f.rgba, w, h), f.delay)).collect();
        let play = Playback::new(frames);
        let still = self.canvas.image();
        self.canvas.set_image(play.frame(), w, h);
        *self.anim.borrow_mut() = Some(Anim { key, play, w, h, still });
        self.arm_anim();
    }

    /// Programme le frame suivant ; sans effet en pause.
    fn arm_anim(self: &Rc<Self>) {
        self.clear_anim_tick();
        let delay = match self.anim.borrow().as_ref() {
            Some(a) if !a.play.paused() => a.play.delay(),
            _ => return,
        };
        let ui = Rc::downgrade(self);
        *self.anim_tick.borrow_mut() = Some(glib::timeout_add_local_once(delay, move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.anim_tick.borrow_mut().take();
            ui.step_anim();
        }));
    }

    fn step_anim(self: &Rc<Self>) {
        let (texture, w, h) = {
            let mut anim = self.anim.borrow_mut();
            let Some(a) = anim.as_mut() else { return };
            (a.play.advance().clone(), a.w, a.h)
        };
        self.canvas.set_image(&texture, w, h);
        self.arm_anim();
    }

    fn clear_anim_tick(&self) {
        if let Some(id) = self.anim_tick.borrow_mut().take() {
            id.remove();
        }
    }

    /// Arrete l'animation : le timer est supprime, les frames libres. `restore` : le canvas retrouve l'image fixe.
    fn stop_anim(&self, restore: bool) {
        self.clear_anim_tick();
        let Some(a) = self.anim.borrow_mut().take() else { return };
        if restore && let Some((texture, w, h)) = a.still {
            self.canvas.set_image(&texture, w, h);
        }
    }

    /// Espace sur un GIF anime : pause et reprise, comme une video.
    fn pause_anim(self: &Rc<Self>) {
        let paused = match self.anim.borrow_mut().as_mut() {
            Some(a) => a.play.toggle_pause(),
            None => return,
        };
        if paused {
            self.clear_anim_tick();
            self.osd.message("pause");
        } else {
            self.osd.message("lecture");
            self.arm_anim();
        }
    }

    /// Dossier sans media : la fenetre est vide, le message reste jusqu'a l'arrivee d'un fichier (`apply`).
    fn show_empty(&self, dir: &Path) {
        let msg = launch::empty_message(dir);
        eprintln!("picme : {msg}");
        self.osd.pin(&msg);
        if let Some(name) = dir.file_name() {
            self.window.set_title(Some(&name.to_string_lossy()));
        }
    }

    /// Ce que les touches voient de la vue.
    fn ctx(&self) -> Ctx {
        Ctx {
            video: self.ctl.borrow().current().is_some_and(|e| e.kind == MediaKind::Video),
            crop: self.cropper.is_active(),
            anim: self.anim.borrow().is_some(),
        }
    }

    /// Lit `path` a `start`, a la place du lecteur en cours.
    fn play(self: &Rc<Self>, path: PathBuf, start: gst::ClockTime, paused: bool) {
        self.stop_player();
        let ui = Rc::downgrade(self);
        let opened = Player::open(&path, self.audio.get(), start, paused, move |event| {
            if let Some(ui) = ui.upgrade() {
                ui.on_player_event(event);
            }
        });
        match opened {
            Ok(player) => {
                self.video_view.set_paintable(Some(player.paintable()));
                self.video_view.set_visible(true);
                *self.player.borrow_mut() = Some((path, player));
            }
            Err(e) => {
                eprintln!("picme : lecture de {} : {e}", path.display());
                self.osd.message(&format!("lecture impossible : {e}"));
            }
        }
    }

    /// Ferme le lecteur : le fichier est relache (corbeille, deplacement, fermeture).
    fn stop_player(&self) {
        let player = self.player.borrow_mut().take();
        if player.is_some() {
            self.video_view.set_paintable(None::<&gdk::Paintable>);
            self.video_view.set_visible(false);
        }
    }

    /// La video courante se lit des qu'elle est la vue posee, pas pendant la repetition d'une touche (les videos
    /// survolees ne s'ouvrent pas) ; sur une autre vue, le lecteur est ferme.
    fn sync_player(self: &Rc<Self>) {
        if self.live_held.get() {
            return;
        }
        let want = {
            let ctl = self.ctl.borrow();
            ctl.current().filter(|e| e.kind == MediaKind::Video && !ctl.repeating()).map(|e| e.path.clone())
        };
        let playing = self.player.borrow().as_ref().map(|(p, _)| p.clone());
        match (want, playing) {
            (Some(w), Some(p)) if w == p => {}
            (Some(w), _) if !self.cropper.is_active() => self.play(w, gst::ClockTime::ZERO, false),
            (None, Some(_)) => self.stop_player(),
            _ => {}
        }
    }

    fn on_player_event(self: &Rc<Self>, event: video::Event) {
        match event {
            video::Event::Eos => {
                if let Some((_, p)) = self.player.borrow().as_ref() {
                    p.finish();
                }
            }
            video::Event::Error(msg) => {
                let Some((path, audio, at, paused)) =
                    self.player.borrow().as_ref().map(|(path, p)| (path.clone(), p.audio(), p.position(), p.is_paused()))
                else {
                    return;
                };
                eprintln!("picme : lecture de {} : {msg}", path.display());
                // la sortie audio manque (ou refuse) : la meme video, sans le son. Hors de l'evenement du bus du lecteur
                // que `play` detruit
                if audio {
                    self.audio.set(false);
                    self.osd.message("sortie audio indisponible : son coupé");
                    let ui = Rc::downgrade(self);
                    glib::idle_add_local_once(move || {
                        if let Some(ui) = ui.upgrade() {
                            ui.play(path, at, paused);
                        }
                    });
                } else {
                    // le lecteur reste en place : `sync_player` ne le rouvrirait que pour echouer de nouveau
                    self.osd.message(&format!("lecture impossible : {msg}"));
                }
            }
        }
    }

    /// Espace sur une video : pause et reprise ; sans lecteur (touche tenue puis relachee), elle se lance. Sur un GIF
    /// anime, c'est l'animation qui se met en pause.
    fn pause(self: &Rc<Self>) {
        if self.anim.borrow().is_some() {
            self.pause_anim();
            return;
        }
        let state = self.player.borrow().as_ref().map(|(_, p)| (p.toggle_pause(), p.position()));
        match state {
            Some((true, at)) => self.osd.message(&format!("pause  {}", video::clock(at))),
            Some((false, _)) => self.osd.message("lecture"),
            None => self.sync_player(),
        }
    }

    /// Maj+fleches : saute de `SEEK_STEP` secondes.
    fn seek(&self, forward: bool) {
        if let Some((_, p)) = self.player.borrow().as_ref() {
            let step = if forward { video::SEEK_STEP } else { -video::SEEK_STEP };
            p.seek_by(step);
            self.osd.message(&format!("{step:+} s"));
        }
    }

    /// M : le son de la video. Le lecteur est recree a la meme position : `playbin` n'ouvre la sortie audio qu'au
    /// demarrage.
    fn mute(self: &Rc<Self>) {
        let audio = !self.audio.get();
        self.audio.set(audio);
        self.osd.message(if audio { "son activé" } else { "son coupé" });
        let state = self.player.borrow().as_ref().map(|(path, p)| (path.clone(), p.position(), p.is_paused()));
        if let Some((path, at, paused)) = state {
            self.play(path, at, paused);
        }
    }

    /// L tenue : le `.mov` de la Live Photo joue par-dessus l'image.
    fn live_start(self: &Rc<Self>) {
        if self.live_held.get() || self.cropper.is_active() {
            return;
        }
        let mov = self.ctl.borrow().current().and_then(|e| e.live_mov.clone());
        if let Some(mov) = mov {
            self.live_held.set(true);
            self.play(mov, gst::ClockTime::ZERO, false);
        }
    }

    /// L relachee : l'image reprend la place.
    fn live_end(self: &Rc<Self>) {
        if self.live_held.replace(false) {
            self.stop_player();
            self.sync_player();
        }
    }

    /// C : le cadre de recadrage, sur l'image au fit telle que le fichier la contient.
    fn crop_start(self: &Rc<Self>) {
        let image = self.ctl.borrow().current().is_some_and(|e| app::decodable(e.kind));
        if !image || !self.canvas.has_image() {
            self.osd.message("recadrage : pas d'image");
        } else if self.canvas.quarters() != 0 {
            self.osd.message("recadrage : la vue est tournée (Ctrl+flèches, ou R en cours)");
        } else {
            // le cadre se pose sur l'image fixe : l'animation s'arrete (et son decode, s'il court encore)
            self.stop_anim(true);
            self.canvas.fit();
            self.cropper.start();
            self.sync_anim();
            self.osd.message("recadrage : Entrée copie, Maj+Entrée remplace, Échap annule");
        }
    }

    /// Echap pendant le crop : le cadre se ferme, le GIF reprend son animation.
    fn crop_cancel(&self) {
        self.cropper.stop();
        self.anim_tried.borrow_mut().take();
        self.sync_anim();
    }

    /// Entree (copie) et Maj+Entree (remplacement, l'original va a la corbeille) : le thread d'ecriture recadre.
    fn crop_apply(&self, replace: bool) {
        let frac = self.cropper.frac();
        self.cropper.stop();
        let Some(path) = self.ctl.borrow().current().map(|e| e.path.clone()) else { return };
        if !self.fileops.crop(path, frac, replace) {
            self.osd.message("recadrage non fait : écritures arrêtées");
        }
    }

    /// Ouvre `path` (fichier depose, copie d'un crop) ; d'un autre dossier, la liste est ce fichier en attendant le
    /// scan de son dossier, qui devient le courant.
    fn open_path(self: &Rc<Self>, path: &Path) {
        let before = self.ctl.borrow().dir().map(Path::to_path_buf);
        let show = self.ctl.borrow_mut().open(path);
        if let Show::Failed(msg) = show {
            eprintln!("picme : {msg}");
            self.osd.message(&msg);
            return;
        }
        if self.ctl.borrow().dir().map(Path::to_path_buf) != before {
            if let Some(dir) = self.ctl.borrow().dir().map(Path::to_path_buf) {
                spawn_scan(dir, self.ctl.borrow().recursive(), self.scan_tx.clone());
            }
            self.watch_dir();
        }
        self.apply(show);
    }

    /// La cible d'un glisser a deplace ces fichiers (action Move reussie) : ils quittent la vue, le `.mov` d'une Live
    /// Photo avant son image (il ne reste pas seul, a se lire).
    fn moved_away(self: &Rc<Self>, paths: Vec<PathBuf>) {
        self.stop_player();
        for path in paths.iter().rev() {
            let show = self.ctl.borrow_mut().reconcile(path);
            self.apply(show);
        }
    }

    /// Vue au fit et sans rotation, plus les rotations R que l'image affichee ne contient pas encore.
    fn reset_view(&self) {
        self.canvas.reset_view();
        self.turned.set(0);
        self.sync_turns();
    }

    /// La vue suit `Controller::view_quarters` : R en cours d'ecriture, ou sur un format qui ne s'ecrit pas.
    fn sync_turns(&self) {
        if !self.canvas.has_image() {
            return;
        }
        let want = self.ctl.borrow_mut().view_quarters();
        // avant `rotate`, qui revient ici par `connect_view` puis `apply`
        let delta = want - self.turned.replace(want);
        if delta != 0 {
            self.canvas.rotate(delta);
        }
    }

    /// R et Maj+R : la vue tourne tout de suite, le fichier ensuite (thread d'ecriture).
    fn turn(self: &Rc<Self>, quarters: i32) {
        let Some(path) = self.ctl.borrow_mut().turn(quarters) else { return };
        self.sync_turns();
        let turn = if quarters > 0 { Orientation::Rotate90 } else { Orientation::Rotate270 };
        if !self.fileops.rotate(path.clone(), turn) {
            let show = self.ctl.borrow_mut().turn_done(&path, false);
            self.apply(show);
            self.osd.message("rotation non enregistrée : écritures arrêtées");
        }
    }

    /// Suppr (corbeille) et Maj+Suppr (corbeille videe apres 30 s) : le courant quitte la liste tout de suite, le disque suit.
    fn remove(self: &Rc<Self>, trash: bool) {
        // le fichier est relache avant la corbeille
        self.stop_player();
        let Some((paths, show)) = self.ctl.borrow_mut().remove_current() else { return };
        self.apply(show);
        let sent = if trash { self.fileops.trash(paths.clone()) } else { self.fileops.delete(paths.clone()) };
        if !sent {
            for path in &paths {
                let show = self.ctl.borrow_mut().removal_done(path);
                self.apply(show);
            }
            self.osd.message("fichier conservé : écritures arrêtées");
        }
    }

    /// Ctrl+Z : le dernier retrait encore dans ses 30 s revient ; la liste suit le compte rendu.
    fn undo(self: &Rc<Self>) {
        if !self.fileops.undo() {
            self.osd.message("annulation impossible : écritures arrêtées");
        }
    }

    /// Compte rendu du thread d'ecriture : la liste suit le disque, la vue du fichier touche repart du fit, l'OSD
    /// le dit.
    fn on_done(self: &Rc<Self>, done: Done) {
        let current = self.ctl.borrow().current().map(|e| e.path.clone());
        let touched = match &done {
            Done::Rotated { path, result } => {
                let show = self.ctl.borrow_mut().turn_done(path, result.is_ok());
                self.apply(show);
                vec![path.clone()]
            }
            Done::Cropped { path, result } => {
                if let Ok(c) = result {
                    let show = self.ctl.borrow_mut().reconcile(&c.path);
                    self.apply(show);
                    // la copie s'ouvre si on est encore sur l'original ; un original remplace garde sa place
                    if !c.replaced && current.as_ref() == Some(path) {
                        self.open_path(&c.path);
                    }
                }
                vec![path.clone()]
            }
            Done::Removed { paths, .. } => {
                for path in paths {
                    let show = self.ctl.borrow_mut().removal_done(path);
                    self.apply(show);
                }
                paths.clone()
            }
            Done::Restored { paths, .. } => {
                for path in paths {
                    let show = self.ctl.borrow_mut().reconcile(path);
                    self.apply(show);
                }
                // on revient sur ce qui vient d'etre remis
                if let Some(first) = paths.first() {
                    self.open_path(first);
                }
                Vec::new()
            }
        };
        if current.is_some_and(|c| touched.contains(&c)) {
            // le fichier est remplace : retour a l'image fixe, l'animation de l'ancienne version est abandonnee
            self.stop_anim(true);
            self.reset_view();
            // `reset_view` ne previent pas le controller : il lache le `Full` d'un zoom en cours
            let show = self.ctl.borrow_mut().set_full(false);
            self.apply(show);
        }
        let msg = done.message();
        if !done.ok() {
            eprintln!("picme : {msg}");
        }
        self.osd.message(&msg);
    }

    /// Nom, position et zoom du courant, un instant par-dessus l'image.
    fn announce(&self) {
        let ctl = self.ctl.borrow();
        let Some(e) = ctl.current() else { return };
        let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let position = ctl.index().map(|i| (i + 1, ctl.entries().len()));
        self.osd.show_info(&osd::format_info(&name, position, self.canvas.percent()));
    }

    /// Le filmstrip suit la liste et le courant du controller.
    fn sync(&self) {
        let ctl = self.ctl.borrow();
        self.strip.sync(ctl.entries(), ctl.rev(), ctl.index());
    }

    fn on_result(self: &Rc<Self>, key: Key, result: picme_core::Result<Frame>) {
        if key.target == Target::Strip {
            self.strip.on_result(key, result);
            return;
        }
        let show = self.ctl.borrow_mut().on_result(key, result);
        self.apply(show);
    }

    fn go(self: &Rc<Self>, delta: isize) {
        let from = self.ctl.borrow().index();
        let show = self.ctl.borrow_mut().go(delta);
        // touche maintenue : les Fit attendent REPEAT_GAP sans navigation (touche tenue en bout de dossier : pas de
        // navigation, le timeout deja arme n'est pas repousse) ; le filmstrip se tait et ne suit plus pendant ce temps,
        // avant `apply` pour que sa synchronisation le sache
        let repeating = self.ctl.borrow().repeating();
        self.strip.pause(repeating);
        self.apply(show);
        let rearm = repeating && self.ctl.borrow().index() != from;
        if rearm {
            self.schedule_settle(app::REPEAT_GAP);
        }
    }

    /// Un autre programme a touche ces fichiers du dossier : la liste suit.
    fn reconcile(self: &Rc<Self>, paths: Vec<PathBuf>) {
        for path in &paths {
            let show = self.ctl.borrow_mut().reconcile(path);
            self.apply(show);
        }
        self.trace.mark(&format!("monitor : {} chemins, {} entrees", paths.len(), self.ctl.borrow().entries().len()));
    }

    fn watch_dir(self: &Rc<Self>) {
        let Some(dir) = self.ctl.borrow().dir().map(|d| d.to_path_buf()) else { return };
        let ui = Rc::downgrade(self);
        match DirWatch::new(&dir, move |paths| {
            if let Some(ui) = ui.upgrade() {
                ui.reconcile(paths);
            }
        }) {
            Ok(w) => *self.watch.borrow_mut() = Some(w),
            Err(e) => eprintln!("picme : {} non surveille : {e}", dir.display()),
        }
    }

    fn act(self: &Rc<Self>, action: Action, main_loop: &glib::MainLoop) {
        match action {
            Action::Next => self.go(1),
            Action::Prev => self.go(-1),
            Action::First => self.go(isize::MIN),
            Action::Last => self.go(isize::MAX),
            // le zoom est celui de l'image : la video, par-dessus, ne le suit pas
            Action::ToggleZoom | Action::ZoomIn | Action::ZoomOut if self.player.borrow().is_some() => {}
            Action::ToggleZoom => self.canvas.toggle_zoom(),
            Action::ZoomIn => self.canvas.zoom_by(ZOOM_STEP),
            Action::ZoomOut => self.canvas.zoom_by(1.0 / ZOOM_STEP),
            Action::Fit => self.canvas.fit(),
            Action::RotateRight => self.canvas.rotate(1),
            Action::RotateLeft => self.canvas.rotate(-1),
            Action::TurnRight => self.turn(1),
            Action::TurnLeft => self.turn(-1),
            Action::OpenFolder => self.open_folder(),
            Action::Help => self.help.toggle(),
            // ouvert, le panneau a deja ferme sur la touche (gestionnaire de touches) : ici il n'est jamais visible
            Action::Info => self.show_info(),
            Action::Map => self.open_map(),
            Action::Trash => self.remove(true),
            Action::Delete => self.remove(false),
            Action::Undo => self.undo(),
            Action::Fullscreen => {
                if self.window.is_fullscreen() {
                    self.window.unfullscreen();
                } else {
                    self.window.fullscreen();
                }
            }
            Action::Quit => {
                self.stop_player();
                self.stop_anim(false);
                main_loop.quit();
            }
            Action::Crop => self.crop_start(),
            Action::CropCancel => self.crop_cancel(),
            Action::CropApply { replace } => self.crop_apply(replace),
            Action::Pause => self.pause(),
            Action::SeekForward => self.seek(true),
            Action::SeekBack => self.seek(false),
            Action::Mute => self.mute(),
            Action::Live => self.live_start(),
        }
    }

    fn schedule_settle(self: &Rc<Self>, after: Duration) {
        if let Some(id) = self.settle.borrow_mut().take() {
            id.remove();
        }
        let ui = Rc::downgrade(self);
        *self.settle.borrow_mut() = Some(glib::timeout_add_local_once(after, move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.settle.borrow_mut().take();
            let show = ui.ctl.borrow_mut().settle();
            ui.apply(show);
            // timeout servi un peu avant REPEAT_GAP apres la derniere navigation : on repasse
            if ui.ctl.borrow().repeating() {
                ui.schedule_settle(Duration::from_millis(5));
            } else {
                ui.strip.pause(false);
            }
        }));
    }

    /// Un tick du banc de navigation. Faux quand il est fini.
    fn bench_tick(self: &Rc<Self>, due: Instant) -> bool {
        let mut done = false;
        if let Some(b) = self.bench.borrow_mut().as_mut() {
            b.late = b.late.max(Instant::now().saturating_duration_since(due));
            b.rss_max_kb = b.rss_max_kb.max(rss_kb());
            if b.waiting.is_some() {
                b.skipped += 1;
            }
            let n = b.done.len() as u32 + b.skipped;
            if n > 0 && n.is_multiple_of(100) {
                b.rss_every_100.push(rss_kb() / 1024);
            }
            if b.left == 0 {
                done = true;
            } else {
                b.left -= 1;
                b.waiting = Some(Instant::now());
            }
        }
        if done {
            self.bench_report();
            return false;
        }
        self.go(1);
        true
    }

    fn bench_report(&self) {
        let Some(b) = self.bench.borrow_mut().take() else { return };
        let mut lat: Vec<f64> = b.done.iter().map(|(d, _)| d.as_secs_f64() * 1000.0).collect();
        lat.sort_by(f64::total_cmp);
        let pct = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(f64::NAN);
        let count = |l: Level| b.done.iter().filter(|(_, x)| *x == l).count();
        eprintln!(
            "bench nav : {} affichees, {} sautees ; latence -> frame peint : p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms ; \
             niveau a l'affichage : Fit {}, Preview {}, Full {} ; tick en retard au plus de {:.1} ms ; VmRSS max {} Mo, fin {} Mo, toutes les 100 : {:?} Mo",
            lat.len(),
            b.skipped,
            pct(0.5),
            pct(0.95),
            pct(1.0),
            count(Level::Fit),
            count(Level::Preview),
            count(Level::Full),
            b.late.as_secs_f64() * 1000.0,
            b.rss_max_kb / 1024,
            rss_kb() / 1024,
            b.rss_every_100,
        );
    }

    fn after_paint(&self) {
        if !self.painted_image.get() && self.canvas.has_image() {
            self.painted_image.set(true);
            self.trace.mark("first frame with image");
        }
        if let Some(b) = self.bench.borrow_mut().as_mut()
            && let Some((t, level)) = b.painting.take()
        {
            b.done.push((t.elapsed(), level));
        }
    }
}

fn main() {
    let trace = Trace { on: std::env::var_os("PICME_TRACE").is_some_and(|v| v == "1"), t0: Instant::now() };
    trace.mark("process start");

    // l'argument d'abord : l'aide, la version et les erreurs de chemin ne demandent ni pool ni fenetre
    let recursive_arg;
    let arg = match launch::parse_args(std::env::args_os().skip(1)) {
        Ok(Cli::Help) => {
            print!("{}", launch::USAGE);
            return;
        }
        Ok(Cli::Version) => {
            println!("{}", launch::version());
            return;
        }
        Ok(Cli::Open { path, recursive }) => {
            recursive_arg = recursive;
            path
        }
        Err(msg) => {
            eprintln!("picme : {msg}\nusage : picme [FICHIER|DOSSIER]... (picme --help)");
            std::process::exit(2);
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        // un chemin absolu n'a pas besoin du dossier courant (supprime sous nos pieds)
        Err(_) if arg.as_ref().is_some_and(|a| a.is_absolute()) => PathBuf::from("/"),
        Err(e) => {
            eprintln!("picme : dossier courant : {}", launch::reason(&e));
            std::process::exit(1);
        }
    };
    let launch = match launch::resolve(arg.as_deref(), &cwd) {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("picme : {msg}");
            std::process::exit(1);
        }
    };

    // pas de GL ni de Wayland : le backend x11 est le seul valide sous i3, et evite la negociation de backends
    // SAFETY : appele avant tout thread (les pools demarrent juste apres) et avant gtk::init()
    unsafe { std::env::set_var("GDK_BACKEND", "x11") };

    // pools et thread d'ecriture avant gtk::init : le Preview de argv[1] se decode pendant l'init GTK
    let (tx, rx) = async_channel::unbounded::<(Key, picme_core::Result<Frame>)>();
    let budget = DEFAULT_BUDGET_BYTES;
    let store = ThumbStore::system();
    let (done_tx, done_rx) = async_channel::unbounded::<Done>();
    let fileops = FileOps::new(store.clone(), done_tx);
    let first_preview = AtomicBool::new(true);
    let thumb_work = app::thumb_work(store, fileops.handle());
    // le pool `thumbs` sert le controller (`Preview`) et le filmstrip (`Strip`) : un seul proprietaire partage
    let thumbs_pool = Rc::new(Pool::new(
        "picme-thumbs",
        pool::THUMB_THREADS,
        move |k: &Key| {
            let t = Instant::now();
            let r = thumb_work(k);
            if first_preview.swap(false, Ordering::Relaxed) {
                trace.mark(&format!("preview decoded ({:.1} ms in worker)", t.elapsed().as_secs_f64() * 1000.0));
            }
            r
        },
        tx.clone(),
    ));
    let decode_pool = Pool::new("picme-decode", pool::decode_threads(), app::decode_work(budget), tx);
    // les frames d'un GIF anime : un seul thread, un seul GIF a la fois (celui du courant)
    let (anim_tx, anim_rx) = async_channel::unbounded::<(Key, picme_core::Result<AnimOutcome>)>();
    let anim_want = anim::Wanted::default();
    let anim_pool = Pool::new("picme-anim", 1, anim::work(anim_want.clone()), anim_tx);
    let mut ctl = Controller::new(budget, Box::new(decode_pool), Box::new(thumbs_pool.clone()), Box::new(canvas::texture_of));
    // un fichier donne s'affiche ; sinon le premier media du dossier, trouve sans attendre le listing complet (le
    // scan ci-dessous le rend), et un dossier sans media s'ouvre vide, surveille
    let mut empty = false;
    ctl.set_recursive(recursive_arg);
    let opened = match &launch.file {
        Some(file) => {
            // `-r` avec un fichier : la liste est l'arbre de son dossier, pas un autre
            if recursive_arg {
                ctl.set_dir_scope(&launch.dir);
            }
            ctl.open(file)
        }
        None => match if recursive_arg { dir::first_tree(&launch.dir) } else { dir::first(&launch.dir) } {
            Ok(dir::First::Found(first)) => {
                trace.mark("first entry found");
                ctl.open(&first.path)
            }
            Ok(dir::First::Empty) => {
                empty = true;
                ctl.open_dir(&launch.dir)
            }
            // trop de fichiers a extension inconnue pour trancher a bon compte : le scan dira (premier media, ou vide)
            Ok(dir::First::Undecided) => ctl.open_dir(&launch.dir),
            Err(e) => {
                eprintln!("picme : {} : {}", launch.dir.display(), launch::reason(&e));
                std::process::exit(1);
            }
        },
    };
    if let Show::Failed(msg) = opened {
        eprintln!("picme : {msg}");
        std::process::exit(1);
    }
    trace.mark("preview submitted");

    // scan du dossier sur un autre thread : la fenetre ne l'attend pas
    let (scan_tx, scan_rx) = async_channel::unbounded::<ScanResult>();
    // metadonnees (I) et page de la carte (G), lues de meme hors du thread d'interface
    let (info_tx, info_rx) = async_channel::unbounded::<(PathBuf, meta::Info)>();
    let (map_tx, map_rx) = async_channel::unbounded::<MapResult>();
    if let Some(d) = ctl.dir().map(|d| d.to_path_buf()) {
        spawn_scan(d, ctl.recursive(), scan_tx.clone());
    }

    if let Err(e) = gtk::init() {
        eprintln!("picme : gtk::init a echoue : {e}");
        std::process::exit(1);
    }
    trace.mark("gtk init");

    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(&gdk::Display::default().expect("display"), &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    let main_loop = glib::MainLoop::new(None, false);
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_default_size(1280, 800);
    let canvas = Canvas::new();
    let strip = Filmstrip::new(thumbs_pool);
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    revealer.set_transition_duration(150);
    revealer.set_halign(gtk::Align::Fill);
    revealer.set_valign(gtk::Align::End);
    revealer.set_child(Some(strip.widget()));
    let osd = Osd::new();
    // la video prend la place de l'image ; jamais sur le chemin d'un clic, d'un glisser ou d'une molette
    let video_view = gtk::Picture::new();
    video_view.set_content_fit(gtk::ContentFit::Contain);
    video_view.set_hexpand(true);
    video_view.set_vexpand(true);
    video_view.set_can_target(false);
    video_view.set_visible(false);
    let live_badge = gtk::Label::new(Some("LIVE"));
    live_badge.add_css_class("live-badge");
    live_badge.set_halign(gtk::Align::Start);
    live_badge.set_valign(gtk::Align::Start);
    live_badge.set_margin_start(12);
    live_badge.set_margin_top(52);
    live_badge.set_can_target(false);
    live_badge.set_visible(false);
    let cropper = Cropper::new({
        let canvas = canvas.clone();
        move || canvas.image_area()
    });
    // le canvas prend toute la fenetre ; la video, le cadre de crop, la bande et l'OSD flottent par-dessus
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&canvas));
    overlay.add_overlay(&video_view);
    overlay.add_overlay(cropper.widget());
    overlay.add_overlay(&live_badge);
    overlay.add_overlay(&revealer);
    osd.attach(&overlay);
    let help = Help::new();
    help.attach(&overlay);
    let info = InfoPanel::new();
    info.attach(&overlay);
    window.set_child(Some(&overlay));
    let chrome = Chrome::new(&window, &revealer);

    let bench = std::env::var("PICME_BENCH_NAV").ok().and_then(|v| {
        let (n, ms) = v.split_once(':').unwrap_or((&v, "40"));
        Some(Bench {
            left: n.parse().ok()?,
            every: Duration::from_millis(ms.parse().ok()?),
            waiting: None,
            painting: None,
            done: Vec::new(),
            skipped: 0,
            late: Duration::ZERO,
            rss_max_kb: 0,
            rss_every_100: Vec::new(),
        })
    });
    let ui = Rc::new(Ui {
        window: window.clone(),
        canvas: canvas.clone(),
        ctl: RefCell::new(ctl),
        trace,
        bench: RefCell::new(bench),
        painted_image: Cell::new(false),
        settle: RefCell::default(),
        osd,
        help,
        info,
        info_want: RefCell::default(),
        info_tx,
        map_busy: Cell::new(false),
        map_tx,
        map_server: RefCell::default(),
        strip: strip.clone(),
        chrome,
        last_current: RefCell::default(),
        watch: RefCell::default(),
        fileops: fileops.handle(),
        turned: Cell::new(0),
        delete_held: Cell::new(None),
        cropper,
        player: RefCell::default(),
        video_view,
        audio: Cell::new(false),
        live_held: Cell::new(false),
        live_badge,
        anim_pool,
        anim_want,
        anim_tried: RefCell::default(),
        anim: RefCell::default(),
        anim_tick: RefCell::default(),
        scan_tx,
    });
    ui.watch_dir();
    if empty {
        ui.show_empty(&launch.dir);
    } else if let Some(notice) = &launch.notice {
        eprintln!("picme : {notice}");
        ui.osd.message(notice);
    }

    dnd::attach_source(
        &canvas,
        {
            let ui = Rc::downgrade(&ui);
            move || ui.upgrade().and_then(|ui| ui.ctl.borrow().current().map(dnd::files_of)).unwrap_or_default()
        },
        {
            let ui = Rc::downgrade(&ui);
            move |paths| {
                if let Some(ui) = ui.upgrade() {
                    ui.moved_away(paths);
                }
            }
        },
    );
    dnd::attach_target(&window, {
        let ui = Rc::downgrade(&ui);
        move |path| {
            if let Some(ui) = ui.upgrade() {
                ui.open_path(&path);
            }
        }
    });

    canvas.connect_view({
        let ui = Rc::downgrade(&ui);
        move |beyond| {
            if let Some(ui) = ui.upgrade() {
                // au-dessus du fit : le controller demande le `Full` ; de retour au fit, il le libere
                let show = ui.ctl.borrow_mut().set_full(beyond);
                ui.apply(show);
                ui.announce();
            }
        }
    });
    canvas.connect_nav({
        let ui = Rc::downgrade(&ui);
        move |delta| {
            if let Some(ui) = ui.upgrade() {
                ui.go(delta);
            }
        }
    });
    strip.connect_activate({
        let ui = Rc::downgrade(&ui);
        move |pos| {
            let Some(ui) = ui.upgrade() else { return };
            let from = ui.ctl.borrow().index();
            if let Some(from) = from {
                ui.go(pos as isize - from as isize);
            }
        }
    });

    // resultats deja prets (le Preview, souvent) : dans le premier frame
    while let Ok((key, result)) = rx.try_recv() {
        ui.on_result(key, result);
    }
    glib::spawn_future_local({
        let ui = Rc::downgrade(&ui);
        async move {
            while let Ok((key, result)) = rx.recv().await {
                let Some(ui) = ui.upgrade() else { break };
                ui.on_result(key, result);
            }
        }
    });
    glib::spawn_future_local({
        let ui = Rc::downgrade(&ui);
        async move {
            while let Ok((key, result)) = anim_rx.recv().await {
                let Some(ui) = ui.upgrade() else { break };
                ui.on_anim(key, result);
            }
        }
    });
    glib::spawn_future_local({
        let ui = Rc::downgrade(&ui);
        async move {
            while let Ok(done) = done_rx.recv().await {
                let Some(ui) = ui.upgrade() else { break };
                ui.on_done(done);
            }
        }
    });
    glib::spawn_future_local({
        let ui = Rc::downgrade(&ui);
        async move {
            while let Ok((d, list)) = scan_rx.recv().await {
                let Some(ui) = ui.upgrade() else { break };
                match list {
                    Ok(list) => {
                        let n = list.len();
                        let show = ui.ctl.borrow_mut().set_entries(&d, list);
                        ui.apply(show);
                        if n == 0 && ui.ctl.borrow().dir() == Some(d.as_path()) {
                            ui.show_empty(&d);
                        }
                        ui.trace.mark(&format!("scan : {n} entrees"));
                    }
                    Err(e) => eprintln!("picme : {} : {e}", d.display()),
                }
            }
        }
    });
    glib::spawn_future_local({
        let ui = Rc::downgrade(&ui);
        async move {
            while let Ok((path, info)) = info_rx.recv().await {
                let Some(ui) = ui.upgrade() else { break };
                ui.on_info(&path, &info);
            }
        }
    });
    glib::spawn_future_local({
        let ui = Rc::downgrade(&ui);
        async move {
            while let Ok(result) = map_rx.recv().await {
                let Some(ui) = ui.upgrade() else { break };
                ui.on_map(result);
            }
        }
    });

    // Fit des la premiere taille connue, puis apres 150 ms sans nouveau resize
    let debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let sized = Cell::new(false);
    canvas.connect_resize({
        let ui = Rc::downgrade(&ui);
        move |w, h| {
            if w == 0 || h == 0 {
                return;
            }
            if !sized.replace(true) {
                if let Some(ui) = ui.upgrade() {
                    let show = ui.ctl.borrow_mut().set_viewport(w, h);
                    ui.apply(show);
                    ui.trace.mark(&format!("viewport {w}x{h}"));
                }
                return;
            }
            if let Some(id) = debounce.borrow_mut().take() {
                id.remove();
            }
            let ui = ui.clone();
            let slot = debounce.clone();
            *debounce.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(150), move || {
                slot.borrow_mut().take();
                if let Some(ui) = ui.upgrade() {
                    let show = ui.ctl.borrow_mut().set_viewport(w, h);
                    ui.apply(show);
                    // une fenetre plus grande peut avoir rendu le zoom inferieur au fit
                    ui.canvas.resync();
                }
            }));
        }
    });

    // en phase Capture : la liste du filmstrip ne doit pas manger les fleches
    let key_ctl = gtk::EventControllerKey::new();
    key_ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
    key_ctl.connect_key_pressed({
        let ui = Rc::downgrade(&ui);
        let main_loop = main_loop.clone();
        move |_, key, code, state| {
            let Some(ui) = ui.upgrade() else { return glib::Propagation::Proceed };
            // l'aide ouverte : la touche la ferme et ne fait rien d'autre
            if ui.help.visible() {
                ui.help.hide();
                return glib::Propagation::Stop;
            }
            // le panneau d'informations de meme : I le ferme comme toute autre touche
            if ui.info.visible() {
                ui.info.hide();
                return glib::Propagation::Stop;
            }
            let Some(action) = keys::action(key, state, ui.ctx()) else { return glib::Propagation::Proceed };
            // Suppr maintenue : une seule fois, jusqu'au relachement
            if matches!(action, Action::Trash | Action::Delete | Action::Undo) {
                if ui.delete_held.get().is_some() {
                    return glib::Propagation::Stop;
                }
                ui.delete_held.set(Some(code));
            }
            ui.act(action, &main_loop);
            glib::Propagation::Stop
        }
    });
    key_ctl.connect_key_released({
        let ui = Rc::downgrade(&ui);
        move |_, key, code, _| {
            let Some(ui) = ui.upgrade() else { return };
            if ui.delete_held.get() == Some(code) {
                ui.delete_held.set(None);
            }
            if key.to_lower() == gdk::Key::l {
                ui.live_end();
            }
        }
    });
    window.add_controller(key_ctl);
    // relachement hors de la fenetre : il n'arrivera pas
    window.connect_is_active_notify({
        let ui = Rc::downgrade(&ui);
        move |w| {
            if let Some(ui) = ui.upgrade()
                && !w.is_active()
            {
                ui.delete_held.set(None);
                ui.live_end();
            }
        }
    });

    window.connect_close_request({
        let main_loop = main_loop.clone();
        move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        }
    });
    window.connect_realize({
        let ui = Rc::downgrade(&ui);
        let main_loop = main_loop.clone();
        move |w| {
            let first = Cell::new(true);
            let ui = ui.clone();
            let main_loop = main_loop.clone();
            w.frame_clock().expect("frame clock").connect_after_paint(move |_| {
                let Some(ui) = ui.upgrade() else { return };
                if first.replace(false) {
                    ui.trace.mark("first frame");
                }
                let starting = !ui.painted_image.get();
                ui.after_paint();
                // banc : demarre quand la premiere image est a l'ecran
                if starting && ui.painted_image.get() && ui.bench.borrow().is_some() {
                    let every = ui.bench.borrow().as_ref().map(|b| b.every).unwrap_or_default();
                    let weak = Rc::downgrade(&ui);
                    let main_loop = main_loop.clone();
                    // glib rearme un timeout a partir de l'heure de dispatch : le retard se mesure tick par tick
                    let last = Rc::new(Cell::new(Instant::now()));
                    glib::timeout_add_local(every, move || {
                        let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break };
                        let due = last.replace(Instant::now()) + every;
                        if ui.bench_tick(due) {
                            glib::ControlFlow::Continue
                        } else {
                            main_loop.quit();
                            glib::ControlFlow::Break
                        }
                    });
                }
            });
        }
    });
    window.present();
    main_loop.run();

    // lecteur ferme (le fichier relache), miniatures en attente ecrites, puis sortie sans attendre les decodes en cours
    ui.stop_player();
    ui.stop_anim(false);
    drop(fileops);
    std::process::exit(0);
}
