//! Bande de miniatures : une `ListView` horizontale virtualisee. `bind` demande la miniature au pool `thumbs`
//! (cle `Target::Strip`, priorite plus basse que toute la navigation), `unbind` retire les demandes encore en
//! file des lignes qui ne sont plus affichees. Les textures vivent dans un LRU de 400. Le filmstrip suit la
//! navigation, et ne demande rien tant qu'une touche est maintenue (`pause`) pour laisser les threads aux `Preview`.

use crate::canvas::texture_of;
use gtk4 as gtk;
use gtk::{gdk, gio, glib, prelude::*};
use picme_core::cache::LruBytes;
use picme_core::decode::{Frame, STRIP_H, STRIP_W, Target};
use picme_core::dir::Entry;
use picme_core::pool::{Key, Pool};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

/// Une cellule : la miniature (jusqu'a 192 x 96, mise a l'echelle) dans une boite fixe, donc des lignes uniformes.
pub const CELL_W: i32 = 112;
pub const CELL_H: i32 = 84;
const CELL_MARGIN: i32 = 3;
/// Miniatures gardees en memoire.
const KEEP: usize = 400;
/// Les cles du filmstrip passent apres celles du controller (0 a 4) ; la plus proche du courant d'abord.
pub const PRIO_STRIP: u32 = 16;

/// Priorite d'une miniature a la position `pos` quand le courant est `current`.
pub fn priority(pos: usize, current: usize) -> u32 {
    PRIO_STRIP + pos.abs_diff(current).min(4096) as u32
}

/// Un remplacement de `remove` elements a partir de `at` par les `add` elements de la nouvelle liste qui suivent `at`.
#[derive(Debug, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub remove: usize,
    pub add: usize,
}

/// Le plus petit remplacement unique qui transforme `old` en `new` (prefixe et suffixe communs conserves) ; `None`
/// si les deux listes sont identiques. Un ajout ou un retrait isole ne touche qu'une ligne.
pub fn diff<T: PartialEq>(old: &[T], new: &[T]) -> Option<Splice> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let room = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    Some(Splice { at: prefix, remove: old.len() - prefix - suffix, add: new.len() - prefix - suffix })
}

pub struct Filmstrip {
    scroller: gtk::ScrolledWindow,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    pool: Rc<Pool<Key>>,
    /// Copie de la liste du controller, alignee ligne a ligne sur `store`.
    entries: RefCell<Vec<Entry>>,
    rev: Cell<u64>,
    current: Cell<usize>,
    textures: RefCell<LruBytes<Key, gdk::Texture>>,
    /// Cles soumises au pool et pas encore revenues ; echecs, pour ne pas les redemander.
    requested: RefCell<HashSet<Key>>,
    failed: RefCell<HashSet<Key>>,
    /// Lignes actuellement liees (celles que la `ListView` affiche, plus sa marge).
    bound: RefCell<Vec<gtk::ListItem>>,
    paused: Cell<bool>,
    /// Courant recu pendant la pause : la selection et le centrage attendent la sortie de pause.
    pending: Cell<Option<usize>>,
    /// Defile a la molette : le filmstrip ne se recentre plus tout seul avant la prochaine navigation.
    user_scrolled: Cell<bool>,
    on_activate: RefCell<Option<OnActivate>>,
}

type OnActivate = Box<dyn Fn(usize)>;

impl Filmstrip {
    pub fn new(pool: Rc<Pool<Key>>) -> Rc<Filmstrip> {
        let store = gio::ListStore::new::<gtk::StringObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_can_unselect(false);
        selection.set_autoselect(false);
        let list = gtk::ListView::new(Some(selection.clone()), None::<gtk::SignalListItemFactory>);
        list.set_orientation(gtk::Orientation::Horizontal);
        list.set_single_click_activate(true);
        list.add_css_class("strip");
        // les fleches sont au controller de la fenetre, pas a la navigation du focus de la liste
        list.set_focusable(false);
        list.set_can_focus(false);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
        scroller.set_min_content_height(CELL_H + 2 * CELL_MARGIN);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&list));
        scroller.set_focusable(false);
        scroller.set_can_focus(false);
        scroller.add_css_class("strip-bar");

        let fs = Rc::new(Filmstrip {
            scroller,
            store,
            selection,
            pool,
            entries: RefCell::default(),
            rev: Cell::new(u64::MAX),
            current: Cell::new(0),
            textures: RefCell::new(LruBytes::new(KEEP * STRIP_W as usize * STRIP_H as usize * 4)),
            requested: RefCell::default(),
            failed: RefCell::default(),
            bound: RefCell::default(),
            paused: Cell::new(false),
            pending: Cell::new(None),
            user_scrolled: Cell::new(false),
            on_activate: RefCell::default(),
        });
        fs.wire(&list);
        fs
    }

    fn wire(self: &Rc<Self>, list: &gtk::ListView) {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let pic = gtk::Picture::new();
            pic.set_content_fit(gtk::ContentFit::Contain);
            pic.set_can_shrink(true);
            pic.set_size_request(CELL_W, CELL_H);
            pic.set_margin_start(CELL_MARGIN);
            pic.set_margin_end(CELL_MARGIN);
            pic.set_margin_top(CELL_MARGIN);
            pic.set_margin_bottom(CELL_MARGIN);
            pic.add_css_class("strip-cell");
            item.set_child(Some(&pic));
        });
        factory.connect_bind({
            let fs = Rc::downgrade(self);
            move |_, item| {
                if let (Some(fs), Some(item)) = (fs.upgrade(), item.downcast_ref::<gtk::ListItem>()) {
                    fs.bound.borrow_mut().push(item.clone());
                    fs.paint(item);
                }
            }
        });
        factory.connect_unbind({
            let fs = Rc::downgrade(self);
            move |_, item| {
                if let (Some(fs), Some(item)) = (fs.upgrade(), item.downcast_ref::<gtk::ListItem>()) {
                    fs.bound.borrow_mut().retain(|b| b != item);
                    fs.abandon();
                }
            }
        });
        list.set_factory(Some(&factory));

        list.connect_activate({
            let fs = Rc::downgrade(self);
            move |_, pos| {
                if let Some(fs) = fs.upgrade()
                    && let Some(cb) = fs.on_activate.borrow().as_ref()
                {
                    cb(pos as usize);
                }
            }
        });

        // molette verticale -> defilement horizontal
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::HORIZONTAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        scroll.connect_scroll({
            let fs = Rc::downgrade(self);
            move |_, dx, dy| {
                let Some(fs) = fs.upgrade() else { return glib::Propagation::Proceed };
                let adj = fs.scroller.hadjustment();
                let d = if dy != 0.0 { dy } else { dx };
                adj.set_value(adj.value() + d * fs.pitch());
                fs.user_scrolled.set(true);
                glib::Propagation::Stop
            }
        });
        self.scroller.add_controller(scroll);

        // les bornes ne sont connues qu'apres la premiere mise en page, et bougent quand la liste change
        self.scroller.hadjustment().connect_changed({
            let fs = Rc::downgrade(self);
            move |_| {
                if let Some(fs) = fs.upgrade()
                    && !fs.user_scrolled.get()
                {
                    fs.center(fs.current.get());
                }
            }
        });
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroller
    }

    /// Un clic sur une miniature : l'index de l'entree.
    pub fn connect_activate(&self, f: impl Fn(usize) + 'static) {
        self.on_activate.replace(Some(Box::new(f)));
    }

    /// Largeur d'une ligne, mesuree sur le defilement (la theme peut ajouter un padding).
    fn pitch(&self) -> f64 {
        let n = self.store.n_items();
        let upper = self.scroller.hadjustment().upper();
        if n > 0 && upper > 0.0 { upper / n as f64 } else { (CELL_W + 2 * CELL_MARGIN) as f64 }
    }

    /// Met la liste au contenu du controller (`rev` : sa version de la liste) et suit l'entree courante.
    pub fn sync(&self, entries: &[Entry], rev: u64, current: Option<usize>) {
        let list_changed = self.rev.replace(rev) != rev;
        if list_changed {
            let splice = diff(&self.entries.borrow(), entries);
            if let Some(s) = splice {
                // la copie d'abord : un `bind` declenche par le splice doit voir la nouvelle liste
                *self.entries.borrow_mut() = entries.to_vec();
                let added: Vec<gtk::StringObject> = entries[s.at..s.at + s.add].iter().map(|e| gtk::StringObject::new(&e.path.to_string_lossy())).collect();
                self.store.splice(s.at as u32, s.remove as u32, &added);
                self.repaint();
            }
        }
        if let Some(i) = current {
            if self.paused.get() {
                // touche maintenue : selectionner et defiler coute plus que la navigation elle-meme
                self.pending.set(Some(i));
            } else if self.current.replace(i) != i || list_changed {
                self.follow(i);
            }
        }
    }

    fn follow(&self, i: usize) {
        self.current.set(i);
        self.user_scrolled.set(false);
        self.selection.set_selected(i as u32);
        self.center(i);
    }

    /// Centre la ligne `i` dans la bande (bornee aux extremites de la liste).
    fn center(&self, i: usize) {
        let adj = self.scroller.hadjustment();
        let v = (i as f64 + 0.5) * self.pitch() - adj.page_size() / 2.0;
        adj.set_value(v.clamp(0.0, (adj.upper() - adj.page_size()).max(0.0)));
    }

    /// Pendant une repetition de touche : aucune demande ; en sortie, celles des lignes affichees.
    pub fn pause(&self, on: bool) {
        if self.paused.replace(on) && !on {
            if let Some(i) = self.pending.take() {
                self.follow(i);
            }
            self.repaint();
        }
    }

    fn key_at(&self, pos: u32) -> Option<Key> {
        self.entries.borrow().get(pos as usize).map(|e| Key::of(e, Target::Strip))
    }

    /// Affiche la miniature de la ligne si elle est en memoire, sinon la demande.
    fn paint(&self, item: &gtk::ListItem) {
        let Some(pic) = item.child().and_downcast::<gtk::Picture>() else { return };
        let pos = item.position();
        let Some(key) = self.key_at(pos) else {
            pic.set_paintable(None::<&gdk::Paintable>);
            return;
        };
        let texture = self.textures.borrow_mut().get(&key).cloned();
        pic.set_paintable(texture.as_ref());
        if texture.is_none() {
            self.request(key, pos as usize);
        }
    }

    fn repaint(&self) {
        let items = self.bound.borrow().clone();
        for item in &items {
            self.paint(item);
        }
    }

    fn request(&self, key: Key, pos: usize) {
        if self.paused.get() || self.failed.borrow().contains(&key) || !self.requested.borrow_mut().insert(key.clone()) {
            return;
        }
        self.pool.submit(key, priority(pos, self.current.get()));
    }

    /// Les lignes qui ne sont plus liees n'ont plus besoin de leur miniature : celles encore en file sont retirees.
    fn abandon(&self) {
        let needed: HashSet<Key> = self.bound.borrow().iter().filter_map(|b| self.key_at(b.position())).collect();
        self.requested.borrow_mut().retain(|k| needed.contains(k));
        self.pool.retain(|k| k.target != Target::Strip || needed.contains(k));
    }

    /// Resultat d'une cle `Strip` du pool.
    pub fn on_result(&self, key: Key, result: picme_core::Result<Frame>) {
        self.requested.borrow_mut().remove(&key);
        match result {
            Ok(frame) => {
                let bytes = frame.bytes();
                self.textures.borrow_mut().insert(key.clone(), texture_of(frame), bytes);
                let items = self.bound.borrow().clone();
                for item in items.iter().filter(|i| self.key_at(i.position()).as_ref() == Some(&key)) {
                    self.paint(item);
                }
            }
            Err(_) => {
                self.failed.borrow_mut().insert(key);
            }
        }
    }

    /// Miniatures en memoire et demandes en cours (diagnostic).
    pub fn stats(&self) -> (usize, usize) {
        (self.textures.borrow().len(), self.requested.borrow().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_lists_need_no_splice() {
        assert_eq!(diff(&[1, 2, 3], &[1, 2, 3]), None);
        assert_eq!(diff::<i32>(&[], &[]), None);
    }

    #[test]
    fn one_insertion_or_removal_touches_one_row() {
        assert_eq!(diff(&[1, 2, 4], &[1, 2, 3, 4]), Some(Splice { at: 2, remove: 0, add: 1 }));
        assert_eq!(diff(&[1, 2, 3, 4], &[1, 2, 4]), Some(Splice { at: 2, remove: 1, add: 0 }));
        assert_eq!(diff(&[1, 2, 3], &[1, 2, 3, 4]), Some(Splice { at: 3, remove: 0, add: 1 }), "a la fin");
        assert_eq!(diff(&[2, 3], &[1, 2, 3]), Some(Splice { at: 0, remove: 0, add: 1 }), "au debut");
        assert_eq!(diff(&[1, 2, 3], &[2, 3]), Some(Splice { at: 0, remove: 1, add: 0 }));
    }

    #[test]
    fn a_modified_row_is_replaced_and_a_new_scan_replaces_the_middle() {
        assert_eq!(diff(&[1, 2, 3], &[1, 9, 3]), Some(Splice { at: 1, remove: 1, add: 1 }));
        assert_eq!(diff(&[3], &[1, 2, 3, 4]), Some(Splice { at: 0, remove: 1, add: 4 }));
        assert_eq!(diff(&[1, 2], &[]), Some(Splice { at: 0, remove: 2, add: 0 }));
        assert_eq!(diff(&[], &[1, 2]), Some(Splice { at: 0, remove: 0, add: 2 }));
        // repetition : le suffixe ne mord pas sur le prefixe
        assert_eq!(diff(&[1, 1], &[1, 1, 1]), Some(Splice { at: 2, remove: 0, add: 1 }));
    }

    #[test]
    fn splice_applied_to_the_old_list_gives_the_new_one() {
        let cases: [(&[i32], &[i32]); 6] = [(&[1, 2, 3], &[1, 3]), (&[1, 2, 3], &[0, 1, 2, 3, 4]), (&[5], &[1, 2, 3]), (&[1, 2, 3, 2, 3], &[1, 2, 3]), (&[1, 2, 3], &[3, 2, 1]), (&[], &[1])];
        for (old, new) in cases {
            let mut v = old.to_vec();
            if let Some(s) = diff(old, new) {
                v.splice(s.at..s.at + s.remove, new[s.at..s.at + s.add].iter().copied());
            }
            assert_eq!(v, new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn closer_thumbnails_come_first_and_all_after_the_navigation() {
        assert!(priority(10, 10) < priority(11, 10));
        assert_eq!(priority(8, 10), priority(12, 10));
        assert!(priority(10, 10) > crate::app::PRIO_FIT + 3, "apres Fit des voisins (au plus 4)");
        assert!(priority(100_000, 0) < u32::MAX);
    }
}
