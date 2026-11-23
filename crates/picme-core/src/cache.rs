//! Cache LRU borne en octets, mono-thread (il vit sur le main thread ; seule la file du pool est partagee).
//!
//! Le budget est dur : apres chaque mutation, la somme des tailles passe sous le budget en evincant le moins
//! recemment utilise. Seule exception, l'entree epinglee (l'image courante, ou le slot `Full`) : jamais evincee,
//! meme si elle depasse a elle seule le budget. Les entrees evincees sont rendues a l'appelant, qui decide de
//! ce qu'il en fait (liberer une texture, journaliser).

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

struct Slot<V> {
    value: V,
    bytes: usize,
    /// Rang de derniere utilisation, cle de `order`.
    tick: u64,
}

pub struct LruBytes<K, V> {
    map: HashMap<K, Slot<V>>,
    /// Entrees par ordre d'utilisation (la plus ancienne en premier) : eviction en O(log n).
    order: BTreeMap<u64, K>,
    tick: u64,
    used: usize,
    budget: usize,
    /// Une seule cle epinglee. C'est une cle et pas une entree : elle reste epinglee si l'entree est retiree
    /// puis reinseree.
    pinned: Option<K>,
}

impl<K: Hash + Eq + Clone, V> LruBytes<K, V> {
    pub fn new(budget: usize) -> Self {
        LruBytes { map: HashMap::new(), order: BTreeMap::new(), tick: 0, used: 0, budget, pinned: None }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Somme des tailles declarees des entrees presentes.
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Lecture sans toucher a l'ordre d'eviction.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.map.get(key).map(|s| &s.value)
    }

    /// Lecture qui marque l'entree comme la plus recemment utilisee.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let tick = self.next_tick();
        let slot = self.map.get_mut(key)?;
        self.order.remove(&slot.tick);
        slot.tick = tick;
        self.order.insert(tick, key.clone());
        Some(&slot.value)
    }

    /// Insere (ou remplace, avec la nouvelle taille) une entree, la plus recente, puis evince jusqu'a tenir dans le
    /// budget. Rend les entrees evincees, la nouvelle comprise si elle depasse seule le budget sans etre epinglee.
    /// La valeur remplacee d'une cle existante n'est pas une eviction : elle est simplement abandonnee.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) -> Vec<(K, V)> {
        self.remove(&key);
        let tick = self.next_tick();
        self.order.insert(tick, key.clone());
        self.map.insert(key, Slot { value, bytes, tick });
        self.used += bytes;
        self.evict()
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.map.remove(key)?;
        self.order.remove(&slot.tick);
        self.used -= slot.bytes;
        Some(slot.value)
    }

    /// Epingle `key` (la precedente cesse de l'etre, d'ou les evictions rendues). La cle n'a pas besoin d'etre presente.
    pub fn pin(&mut self, key: K) -> Vec<(K, V)> {
        self.pinned = Some(key);
        self.evict()
    }

    pub fn unpin(&mut self) -> Vec<(K, V)> {
        self.pinned = None;
        self.evict()
    }

    /// Change le budget et evince si le nouveau est plus petit.
    pub fn set_budget(&mut self, budget: usize) -> Vec<(K, V)> {
        self.budget = budget;
        self.evict()
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Evince les plus anciennes, hors epinglee, jusqu'a passer sous le budget (ou ne plus avoir que l'epinglee).
    fn evict(&mut self) -> Vec<(K, V)> {
        let mut evicted = Vec::new();
        while self.used > self.budget {
            let Some(key) = self.order.values().find(|k| self.pinned.as_ref() != Some(*k)).cloned() else {
                break;
            };
            let value = self.remove(&key).expect("la cle vient de `order`");
            evicted.push((key, value));
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(evicted: &[(&'static str, u32)]) -> Vec<&'static str> {
        evicted.iter().map(|(k, _)| *k).collect()
    }

    #[test]
    fn evicts_oldest_until_under_budget() {
        let mut c = LruBytes::new(100);
        assert!(c.insert("a", 1, 40).is_empty());
        assert!(c.insert("b", 2, 40).is_empty());
        assert_eq!(c.used_bytes(), 80);
        // 120 > 100 : "a" part ; 80 <= 100, on s'arrete
        assert_eq!(keys(&c.insert("c", 3, 40)), ["a"]);
        assert_eq!((c.len(), c.used_bytes()), (2, 80));
        assert!(!c.contains(&"a") && c.contains(&"b") && c.contains(&"c"));
        // une grosse entree en evince plusieurs, dans l'ordre
        let out = c.insert("d", 4, 90);
        assert_eq!(keys(&out), ["b", "c"]);
        assert_eq!(out[0].1, 2, "la valeur evincee est rendue");
        assert_eq!((c.len(), c.used_bytes()), (1, 90));
    }

    #[test]
    fn get_bumps_but_peek_does_not() {
        let mut c = LruBytes::new(100);
        c.insert("a", 1, 40);
        c.insert("b", 2, 40);
        assert_eq!(c.peek(&"a"), Some(&1));
        assert_eq!(keys(&c.insert("c", 3, 40)), ["a"], "peek n'a rien change");

        let mut c = LruBytes::new(100);
        c.insert("a", 1, 40);
        c.insert("b", 2, 40);
        assert_eq!(c.get(&"a"), Some(&1));
        assert_eq!(keys(&c.insert("c", 3, 40)), ["b"], "get a rajeuni a");
        assert_eq!(c.get(&"zzz"), None);
    }

    #[test]
    fn reinserting_a_key_replaces_it_and_fixes_the_accounting() {
        let mut c = LruBytes::new(100);
        c.insert("a", 1, 40);
        c.insert("b", 2, 40);
        // "a" passe a 50 octets : 90 au total, rien n'est evince, et l'ancienne taille n'est plus comptee
        assert!(c.insert("a", 10, 50).is_empty());
        assert_eq!((c.len(), c.used_bytes()), (2, 90));
        assert_eq!(c.peek(&"a"), Some(&10));
        // "a" est desormais la plus recente : "b" part en premier
        assert_eq!(keys(&c.insert("c", 3, 20)), ["b"]);
        assert_eq!(c.used_bytes(), 70);
        // l'ordre ne garde pas de doublon : retirer toutes les cles remet le compte a zero
        for k in ["a", "c"] {
            c.remove(&k);
        }
        assert_eq!((c.len(), c.used_bytes()), (0, 0));
        assert!(c.order.is_empty());
    }

    #[test]
    fn pinned_entry_is_never_evicted_even_over_budget() {
        let mut c = LruBytes::new(100);
        c.pin("cur");
        // plus grosse que le budget a elle seule, mais epinglee : elle reste
        assert!(c.insert("cur", 0, 250).is_empty());
        assert_eq!((c.len(), c.used_bytes()), (1, 250));
        // les autres entrees sont evincees pour tenter de rentrer, jamais "cur"
        let out = c.insert("x", 1, 10);
        assert_eq!(keys(&out), ["x"], "l'entree non epinglee qui depasse le budget est evincee elle-meme");
        assert!(c.contains(&"cur"));
        // epinglee la plus ancienne, avec de la place pour une autre : l'eviction saute l'epinglee
        let mut c = LruBytes::new(100);
        c.insert("cur", 0, 60);
        c.pin("cur");
        c.insert("a", 1, 30);
        assert_eq!(keys(&c.insert("b", 2, 30)), ["a"]);
        assert!(c.contains(&"cur") && c.contains(&"b"));
    }

    #[test]
    fn unpin_and_repin_release_the_old_entry() {
        let mut c = LruBytes::new(100);
        c.pin("cur");
        c.insert("cur", 0, 150);
        c.insert("a", 1, 10);
        assert_eq!((c.len(), c.used_bytes()), (1, 150), "a n'a pas tenu dans le budget");
        // on epingle une autre cle : "cur" redevient evincable et part tout de suite
        assert_eq!(keys(&c.pin("next")), ["cur"]);
        assert!(c.is_empty());
        // la cle epinglee l'est aussi quand l'entree arrive apres
        c.insert("next", 2, 200);
        assert!(c.contains(&"next"));
        assert_eq!(keys(&c.unpin()), ["next"]);
    }

    #[test]
    fn set_budget_shrinks_immediately() {
        let mut c = LruBytes::new(100);
        for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
            c.insert(k, v, 30);
        }
        assert_eq!(keys(&c.set_budget(50)), ["a", "b"]);
        assert_eq!((c.budget(), c.used_bytes()), (50, 30));
        assert!(c.set_budget(500).is_empty());
    }

    #[test]
    fn stays_consistent_under_a_long_mixed_workload() {
        // reference naive : vecteur ordonne de (cle, octets), le plus ancien en tete
        let budget = 1000;
        let mut c: LruBytes<u32, u32> = LruBytes::new(budget);
        let mut model: Vec<(u32, usize)> = Vec::new();
        let mut seed = 12345u32;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) % n
        };
        for _ in 0..5000 {
            let key = rnd(30);
            if rnd(3) == 0 {
                let hit = c.get(&key).is_some();
                assert_eq!(hit, model.iter().any(|(k, _)| *k == key));
                if let Some(i) = model.iter().position(|(k, _)| *k == key) {
                    let e = model.remove(i);
                    model.push(e);
                }
            } else {
                let bytes = 1 + rnd(300) as usize;
                c.insert(key, key, bytes);
                model.retain(|(k, _)| *k != key);
                model.push((key, bytes));
                while model.iter().map(|(_, b)| b).sum::<usize>() > budget {
                    model.remove(0);
                }
            }
            assert_eq!(c.used_bytes(), model.iter().map(|(_, b)| b).sum::<usize>());
            assert_eq!(c.len(), model.len());
            assert!(c.used_bytes() <= budget);
        }
    }
}
