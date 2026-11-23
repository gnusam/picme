use gtk4 as gtk;
use gtk::{gdk, glib, prelude::*};
use std::{cell::Cell, rc::Rc, time::Instant};

// dependances declarees d'avance (core : bloc 2, gstreamer : bloc 6), pas encore utilisees
use {gstgtk4 as _, gstreamer as _, picme_core as _};

/// `PICME_TRACE=1` : horodatages sur stderr, en ms depuis le debut de `main`.
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

fn main() {
    let trace = Trace { on: std::env::var_os("PICME_TRACE").is_some_and(|v| v == "1"), t0: Instant::now() };
    trace.mark("process start");

    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage : picme <image.jpg>");
        std::process::exit(2);
    };

    // pas de GL ni de Wayland : le backend x11 est le seul valide sous i3, et evite la negociation de backends
    // SAFETY : appele avant tout thread et avant gtk::init()
    unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    if let Err(e) = gtk::init() {
        eprintln!("picme : gtk::init a echoue : {e}");
        std::process::exit(1);
    }
    trace.mark("gtk init");

    let css = gtk::CssProvider::new();
    css.load_from_string("window { background: #000; }");
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    match gdk::Texture::from_filename(&path) {
        Ok(texture) => picture.set_paintable(Some(&texture)),
        Err(e) => eprintln!("picme : {} : {e}", path.to_string_lossy()),
    }
    trace.mark("decode done");

    let main_loop = glib::MainLoop::new(None, false);
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_default_size(1280, 800);
    window.set_child(Some(&picture));
    window.connect_close_request({
        let main_loop = main_loop.clone();
        move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        }
    });
    if trace.on {
        // premier frame = premier after-paint de l'horloge de la fenetre
        let trace = Rc::new(trace);
        window.connect_realize(move |w| {
            let trace = trace.clone();
            let first = Cell::new(true);
            w.frame_clock().expect("frame clock").connect_after_paint(move |_| {
                if first.replace(false) {
                    trace.mark("first frame");
                }
            });
        });
    }
    window.present();
    main_loop.run();
}
