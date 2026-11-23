//! Pool de threads a file prioritaire (`Mutex` + `Condvar`), resultats rendus par `async_channel`.
//!
//! La file est la seule structure partagee avec le main thread. Un job est une cle : la fonction de travail, fixee a
//! la creation du pool, en deduit tout ce qu'il faut. Une cle deja en file n'est pas dupliquee (sa priorite est
//! remplacee) ; une cle en cours d'execution n'est pas remise en file (son resultat arrive). `retain` retire de la
//! file les jobs devenus inutiles ; un job deja lance va jusqu'au bout et c'est au destinataire de trier ce qu'il
//! recoit. A l'arret (`Drop`), la file est videe et chaque worker termine son job en cours avant d'etre joint.

use crate::decode::Target;
use crate::dir::Entry;
use crate::error::{Error, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// Cle d'un job de decode : fichier, version du fichier, taille voulue. `mtime_ns` et `size` rendent inoffensif un
/// resultat arrive apres une modification du fichier : il ne correspond plus a l'entree courante.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub path: PathBuf,
    pub mtime_ns: i64,
    pub size: u64,
    pub target: Target,
}

impl Key {
    pub fn of(entry: &Entry, target: Target) -> Key {
        Key { path: entry.path.clone(), mtime_ns: entry.mtime_ns, size: entry.size, target }
    }
}

/// Threads du pool `decode` : la moitie des coeurs, entre 2 et 4.
pub fn decode_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    (cores / 2).clamp(2, 4)
}

/// Threads du pool `thumbs`.
pub const THUMB_THREADS: usize = 2;

struct State<K> {
    /// Jobs en attente par (priorite, numero d'arrivee) : le plus petit sort en premier, FIFO a priorite egale.
    queue: BTreeMap<(u32, u64), K>,
    /// Index inverse de `queue`, pour la deduplication et `retain`.
    queued: HashMap<K, (u32, u64)>,
    running: HashSet<K>,
    seq: u64,
    stop: bool,
}

struct Shared<K> {
    state: Mutex<State<K>>,
    ready: Condvar,
}

impl<K> Shared<K> {
    /// Verrou tolerant au poison : le travail tourne hors du verrou, seul un predicat de `retain` qui panique
    /// pourrait l'empoisonner, et l'etat reste coherent dans ce cas (chaque mutation est faite en une fois).
    fn lock(&self) -> MutexGuard<'_, State<K>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Pool<K> {
    shared: Arc<Shared<K>>,
    threads: Vec<JoinHandle<()>>,
}

impl<K: Clone + Eq + Hash + Send + 'static> Pool<K> {
    /// Lance `threads` workers nommes `picme-<name>-<i>`. Chaque job execute `work(&cle)` et envoie `(cle, resultat)`
    /// sur `tx` ; une panique du travail devient une `Error::Decode`, le worker continue.
    pub fn new<T, F>(name: &str, threads: usize, work: F, tx: async_channel::Sender<(K, Result<T>)>) -> Pool<K>
    where
        T: Send + 'static,
        F: Fn(&K) -> Result<T> + Send + Sync + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { queue: BTreeMap::new(), queued: HashMap::new(), running: HashSet::new(), seq: 0, stop: false }),
            ready: Condvar::new(),
        });
        let work = Arc::new(work);
        let threads = (0..threads.max(1))
            .map(|i| {
                let (shared, work, tx) = (shared.clone(), work.clone(), tx.clone());
                std::thread::Builder::new()
                    .name(format!("picme-{name}-{i}"))
                    .spawn(move || worker(&shared, &*work, &tx))
                    .expect("lancement d'un thread du pool")
            })
            .collect();
        Pool { shared, threads }
    }

    /// Met `key` en file avec `priority` (la plus petite passe en premier). Rend `true` si un nouveau job est cree ;
    /// `false` si la cle est deja en file (sa priorite devient `priority`, son rang d'arrivee ne change pas), deja en
    /// cours, ou si le pool s'arrete.
    pub fn submit(&self, key: K, priority: u32) -> bool {
        let mut st = self.shared.lock();
        if st.stop || st.running.contains(&key) {
            return false;
        }
        if let Some(&(old, seq)) = st.queued.get(&key) {
            if old != priority {
                st.queue.remove(&(old, seq));
                st.queue.insert((priority, seq), key.clone());
                st.queued.insert(key, (priority, seq));
            }
            return false;
        }
        st.seq += 1;
        let seq = st.seq;
        st.queue.insert((priority, seq), key.clone());
        st.queued.insert(key, (priority, seq));
        drop(st);
        self.shared.ready.notify_one();
        true
    }

    /// Retire de la file les jobs dont la cle ne satisfait pas `keep` (appele sous le verrou : il doit etre court).
    /// Les jobs en cours ne sont pas touches. Rend le nombre de jobs retires.
    pub fn retain(&self, mut keep: impl FnMut(&K) -> bool) -> usize {
        let mut st = self.shared.lock();
        let State { queue, queued, .. } = &mut *st;
        let before = queued.len();
        queued.retain(|k, slot| {
            let kept = keep(k);
            if !kept {
                queue.remove(slot);
            }
            kept
        });
        before - queued.len()
    }

    /// Jobs en file plus jobs en cours.
    pub fn pending(&self) -> usize {
        let st = self.shared.lock();
        st.queued.len() + st.running.len()
    }
}

impl<K> Drop for Pool<K> {
    fn drop(&mut self) {
        {
            let mut st = self.shared.lock();
            st.stop = true;
            st.queue.clear();
            st.queued.clear();
        }
        self.shared.ready.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn worker<K, T>(shared: &Shared<K>, work: &(dyn Fn(&K) -> Result<T> + Send + Sync), tx: &async_channel::Sender<(K, Result<T>)>)
where
    K: Clone + Eq + Hash,
{
    loop {
        let key = {
            let mut st = shared.lock();
            loop {
                if st.stop {
                    return;
                }
                if let Some((_, key)) = st.queue.pop_first() {
                    st.queued.remove(&key);
                    st.running.insert(key.clone());
                    break key;
                }
                st = shared.ready.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let result = catch_unwind(AssertUnwindSafe(|| work(&key))).unwrap_or_else(|p| {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned());
            Err(Error::Decode(format!("panique : {}", msg.unwrap_or_default())))
        });
        // retrait de `running` et envoi sous le meme verrou : un `submit` concurrent voit soit le job en cours (et
        // recevra ce resultat), soit le job fini avec son resultat deja dans le canal, jamais un trou entre les deux.
        // Canal non borne : `try_send` n'echoue que si le destinataire a disparu.
        let mut st = shared.lock();
        st.running.remove(&key);
        let _ = tx.try_send((key, result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Execute `f` sur un thread et echoue si elle ne rend pas la main dans `secs` secondes (deadlock).
    fn within<R: Send + 'static>(secs: u64, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs)).expect("pas de reponse dans le delai : deadlock ?")
    }

    /// Porte qui bloque le job "gate" jusqu'a `open`.
    struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
        entered: AtomicBool,
    }

    impl Gate {
        fn new() -> Arc<Gate> {
            Arc::new(Gate { open: Mutex::new(false), cv: Condvar::new(), entered: AtomicBool::new(false) })
        }

        fn pass(&self) {
            self.entered.store(true, Ordering::SeqCst);
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.cv.wait(open).unwrap();
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }

        /// Attend qu'un worker soit entre dans le job "gate".
        fn wait_entered(&self) {
            let t0 = Instant::now();
            while !self.entered.load(Ordering::SeqCst) {
                assert!(t0.elapsed() < Duration::from_secs(5), "le job gate n'a jamais demarre");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    type Results = async_channel::Receiver<(String, Result<usize>)>;

    /// Pool d'un thread dont le travail note l'ordre d'execution ; la cle "gate" bloque jusqu'a `Gate::open`, la cle
    /// "boom" panique.
    fn recording_pool(gate: &Arc<Gate>) -> (Pool<String>, Arc<Mutex<Vec<String>>>, Results) {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = async_channel::unbounded();
        let (g, o) = (gate.clone(), order.clone());
        let pool = Pool::new(
            "test",
            1,
            move |k: &String| {
                match k.as_str() {
                    "gate" => g.pass(),
                    "boom" => panic!("boom"),
                    _ => {}
                }
                o.lock().unwrap().push(k.clone());
                Ok(k.len())
            },
            tx,
        );
        (pool, order, rx)
    }

    fn recv_n<T>(rx: &async_channel::Receiver<T>, n: usize) -> Vec<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        while out.len() < n {
            match rx.try_recv() {
                Ok(v) => out.push(v),
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => panic!("{} resultats sur {n} : {e}", out.len()),
            }
        }
        out
    }

    #[test]
    fn runs_by_priority_then_arrival_order() {
        within(10, || {
            let gate = Gate::new();
            let (pool, order, rx) = recording_pool(&gate);
            assert!(pool.submit("gate".into(), 0));
            gate.wait_entered();
            for (k, p) in [("p5", 5), ("p1a", 1), ("p3", 3), ("p1b", 1), ("p0", 0), ("p2", 2)] {
                assert!(pool.submit(k.into(), p));
            }
            gate.open();
            recv_n(&rx, 7);
            assert_eq!(*order.lock().unwrap(), ["gate", "p0", "p1a", "p1b", "p2", "p3", "p5"]);
        });
    }

    #[test]
    fn deduplicates_queued_and_running_keys_and_updates_priority() {
        within(10, || {
            let gate = Gate::new();
            let (pool, order, rx) = recording_pool(&gate);
            assert!(pool.submit("gate".into(), 0));
            gate.wait_entered();
            assert!(!pool.submit("gate".into(), 0), "deja en cours : pas de second job");
            assert!(pool.submit("b".into(), 5));
            assert!(pool.submit("c".into(), 3));
            assert!(!pool.submit("b".into(), 1), "deja en file : priorite mise a jour, pas de doublon");
            assert!(!pool.submit("c".into(), 3));
            assert_eq!(pool.pending(), 3);
            gate.open();
            recv_n(&rx, 3);
            assert_eq!(*order.lock().unwrap(), ["gate", "b", "c"]);
            std::thread::sleep(Duration::from_millis(20));
            assert!(rx.try_recv().is_err(), "aucun resultat en double");
            assert_eq!(pool.pending(), 0);
            // une cle terminee peut etre redemandee
            assert!(pool.submit("b".into(), 0));
            recv_n(&rx, 1);
        });
    }

    #[test]
    fn retain_drops_queued_jobs_but_not_the_running_one() {
        within(10, || {
            let gate = Gate::new();
            let (pool, order, rx) = recording_pool(&gate);
            pool.submit("gate".into(), 0);
            gate.wait_entered();
            for i in 0..10 {
                pool.submit(format!("k{i}"), i);
            }
            let removed = pool.retain(|k| k == "gate" || k.trim_start_matches('k').parse::<u32>().unwrap() % 2 == 1);
            assert_eq!(removed, 5);
            assert_eq!(pool.retain(|k| k != "gate"), 0, "le job en cours n'est pas dans la file");
            gate.open();
            recv_n(&rx, 6);
            assert_eq!(*order.lock().unwrap(), ["gate", "k1", "k3", "k5", "k7", "k9"]);
        });
    }

    #[test]
    fn a_panicking_job_is_an_error_and_the_worker_survives() {
        within(10, || {
            let gate = Gate::new();
            gate.open();
            let (pool, _, rx) = recording_pool(&gate);
            pool.submit("boom".into(), 0);
            pool.submit("after".into(), 1);
            let got = recv_n(&rx, 2);
            assert_eq!(got[0].0, "boom");
            assert!(matches!(&got[0].1, Err(Error::Decode(m)) if m.contains("boom")), "{:?}", got[0].1);
            assert_eq!((got[1].0.as_str(), got[1].1.as_ref().ok()), ("after", Some(&5)));
        });
    }

    #[test]
    fn drop_waits_for_the_running_job_and_discards_the_queue() {
        within(10, || {
            let gate = Gate::new();
            let (pool, order, rx) = recording_pool(&gate);
            pool.submit("gate".into(), 0);
            gate.wait_entered();
            for i in 0..100 {
                pool.submit(format!("q{i}"), 1);
            }
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                drop(pool);
                done_tx.send(()).unwrap();
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(done_rx.try_recv().is_err(), "le drop attend la fin du job en cours");
            gate.open();
            done_rx.recv_timeout(Duration::from_secs(5)).expect("arret bloque");
            assert_eq!(*order.lock().unwrap(), ["gate"], "aucun job de la file n'a tourne apres l'arret");
            assert_eq!(recv_n(&rx, 1)[0].0, "gate");
            assert!(rx.recv_blocking().is_err(), "canal ferme : tous les workers sont partis");
        });
    }

    #[test]
    fn idle_pool_stops_immediately() {
        within(5, || {
            let (tx, _rx) = async_channel::unbounded::<(u32, Result<()>)>();
            let pool = Pool::new("idle", 4, |_: &u32| Ok(()), tx);
            drop(pool);
        });
    }

    #[test]
    fn stress_concurrent_submit_and_retain_loses_and_duplicates_nothing() {
        within(60, || {
            const KEYS: usize = 3000;
            const SUBMITTERS: usize = 4;
            const PER_SUBMITTER: usize = 2500; // 10 000 submit au total
            let accepted: Arc<Vec<AtomicU32>> = Arc::new((0..KEYS).map(|_| AtomicU32::new(0)).collect());
            let removed: Arc<Vec<AtomicU32>> = Arc::new((0..KEYS).map(|_| AtomicU32::new(0)).collect());
            let in_flight = Arc::new(Mutex::new(HashSet::new()));
            let (tx, rx) = async_channel::unbounded();
            let flight = in_flight.clone();
            let pool = Arc::new(Pool::new(
                "stress",
                4,
                move |k: &usize| {
                    // la meme cle chez deux workers a la fois serait un doublon
                    assert!(flight.lock().unwrap().insert(*k), "cle {k} executee deux fois en meme temps");
                    let mut x = *k as u64;
                    for _ in 0..(200 + k % 500) {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    }
                    flight.lock().unwrap().remove(k);
                    Ok(x)
                },
                tx,
            ));

            let done = Arc::new(AtomicBool::new(false));
            let retainer = {
                let (pool, removed, done) = (pool.clone(), removed.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut round = 0usize;
                    while !done.load(Ordering::SeqCst) {
                        round += 1;
                        pool.retain(|k| {
                            let keep = !(k + round).is_multiple_of(7);
                            if !keep {
                                removed[*k].fetch_add(1, Ordering::SeqCst);
                            }
                            keep
                        });
                        std::thread::yield_now();
                    }
                })
            };
            let submitters: Vec<_> = (0..SUBMITTERS)
                .map(|s| {
                    let (pool, accepted) = (pool.clone(), accepted.clone());
                    std::thread::spawn(move || {
                        let mut seed = 0x9E37_79B9u32.wrapping_mul(s as u32 + 1);
                        for i in 0..PER_SUBMITTER {
                            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                            let k = (seed >> 8) as usize % KEYS;
                            if pool.submit(k, (seed % 5) + (i % 3) as u32) {
                                accepted[k].fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    })
                })
                .collect();
            for t in submitters {
                t.join().unwrap();
            }
            done.store(true, Ordering::SeqCst);
            retainer.join().unwrap();

            let t0 = Instant::now();
            while pool.pending() > 0 {
                assert!(t0.elapsed() < Duration::from_secs(30), "pool jamais vide : {} jobs", pool.pending());
                std::thread::sleep(Duration::from_millis(2));
            }
            let pool = Arc::into_inner(pool).expect("plus aucun autre detenteur du pool");
            drop(pool);

            let mut executed = vec![0u32; KEYS];
            while let Ok((k, r)) = rx.try_recv() {
                r.unwrap();
                executed[k] += 1;
            }
            assert!(rx.is_closed() || rx.is_empty());
            let (mut total_acc, mut total_rem, mut total_exec) = (0, 0, 0);
            for k in 0..KEYS {
                let (a, r, e) = (accepted[k].load(Ordering::SeqCst), removed[k].load(Ordering::SeqCst), executed[k]);
                assert_eq!(a, r + e, "cle {k} : {a} jobs crees, {r} retires, {e} executes");
                total_acc += a;
                total_rem += r;
                total_exec += e;
            }
            eprintln!("stress : {total_acc} jobs crees, {total_rem} retires, {total_exec} executes");
            assert!(total_rem > 0 && total_exec > 0, "le test doit exercer les deux chemins");
        });
    }
}
