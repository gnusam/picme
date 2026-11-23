use gtk4 as gtk;
use gtk::{gdk, glib, prelude::*};
use picme::app::{self, Controller, Level, Show};
use picme::canvas::{self, Canvas};
use picme::chrome::Chrome;
use picme::fileops::FileOps;
use picme::filmstrip::Filmstrip;
use picme::keys::{self, Action};
use picme::monitor::DirWatch;
use picme::osd::{self, Osd};
use picme::view::ZOOM_STEP;
use picme_core::decode::{DEFAULT_BUDGET_BYTES, Frame, Target};
use picme_core::dir::{self, Entry};
use picme_core::pool::{self, Key, Pool};
use picme_core::thumbs::{self, ThumbStore};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// dependances declarees d'avance (gstreamer : bloc 6), pas encore utilisees
use {gstgtk4 as _, gstreamer as _};

const CSS: &str = "
window { background: #000; }
.osd { background: rgba(0, 0, 0, 0.6); color: #eee; padding: 4px 10px; border-radius: 6px; font-size: 13px; }
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
    strip: Rc<Filmstrip>,
    chrome: Rc<Chrome>,
    /// Le courant a la derniere mise a jour : un autre fichier remet la vue au fit.
    last_current: RefCell<Option<PathBuf>>,
    watch: RefCell<Option<DirWatch>>,
}

impl Ui {
    fn apply(&self, show: Show<gdk::Texture>) {
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
        if let Some(path) = &current {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if self.window.title().as_deref() != Some(name.as_str()) {
                self.window.set_title(Some(&name));
            }
        }
        if *self.last_current.borrow() != current {
            *self.last_current.borrow_mut() = current;
            self.canvas.reset_view();
            self.chrome.pulse();
            self.announce();
        }
        self.sync();
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

    fn on_result(&self, key: Key, result: picme_core::Result<Frame>) {
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
    fn reconcile(&self, paths: Vec<PathBuf>) {
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
            Action::ToggleZoom => self.canvas.toggle_zoom(),
            Action::ZoomIn => self.canvas.zoom_by(ZOOM_STEP),
            Action::ZoomOut => self.canvas.zoom_by(1.0 / ZOOM_STEP),
            Action::Fit => self.canvas.fit(),
            Action::RotateRight => self.canvas.rotate(1),
            Action::RotateLeft => self.canvas.rotate(-1),
            Action::Fullscreen => {
                if self.window.is_fullscreen() {
                    self.window.unfullscreen();
                } else {
                    self.window.fullscreen();
                }
            }
            Action::Quit => main_loop.quit(),
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

    // pas de GL ni de Wayland : le backend x11 est le seul valide sous i3, et evite la negociation de backends
    // SAFETY : appele avant tout thread (les pools demarrent juste apres) et avant gtk::init()
    unsafe { std::env::set_var("GDK_BACKEND", "x11") };

    let Some(arg) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage : picme <image>");
        std::process::exit(2);
    };

    // pools et thread d'ecriture avant gtk::init : le Preview de argv[1] se decode pendant l'init GTK
    let (tx, rx) = async_channel::unbounded::<(Key, picme_core::Result<Frame>)>();
    let budget = DEFAULT_BUDGET_BYTES;
    let store = thumbs::default_root().map(ThumbStore::new);
    let fileops = FileOps::new(store.clone());
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
    let mut ctl = Controller::new(budget, Box::new(decode_pool), Box::new(thumbs_pool.clone()), Box::new(canvas::texture_of));
    if let Show::Failed(msg) = ctl.open(&arg) {
        eprintln!("picme : {msg}");
        std::process::exit(1);
    }
    trace.mark("preview submitted");

    // scan du dossier sur un autre thread : la fenetre ne l'attend pas
    let (scan_tx, scan_rx) = async_channel::bounded::<(PathBuf, std::io::Result<Vec<Entry>>)>(1);
    if let Some(d) = ctl.dir().map(|d| d.to_path_buf()) {
        std::thread::Builder::new()
            .name("picme-scan".into())
            .spawn(move || {
                let list = dir::list(&d);
                let _ = scan_tx.send_blocking((d, list));
            })
            .expect("thread scan");
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
    // le canvas prend toute la fenetre ; la bande et l'OSD flottent par-dessus
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&canvas));
    overlay.add_overlay(&revealer);
    osd.attach(&overlay);
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
        strip: strip.clone(),
        chrome,
        last_current: RefCell::default(),
        watch: RefCell::default(),
    });
    ui.watch_dir();

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
            if let Ok((d, list)) = scan_rx.recv().await
                && let Some(ui) = ui.upgrade()
            {
                match list {
                    Ok(list) => {
                        let n = list.len();
                        let show = ui.ctl.borrow_mut().set_entries(&d, list);
                        ui.apply(show);
                        ui.trace.mark(&format!("scan : {n} entrees"));
                    }
                    Err(e) => eprintln!("picme : {} : {e}", d.display()),
                }
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
        move |_, key, _, state| {
            let (Some(ui), Some(action)) = (ui.upgrade(), keys::action(key, state)) else { return glib::Propagation::Proceed };
            ui.act(action, &main_loop);
            glib::Propagation::Stop
        }
    });
    window.add_controller(key_ctl);

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

    // miniatures en attente ecrites, puis sortie sans attendre les decodes en cours
    drop(fileops);
    std::process::exit(0);
}
