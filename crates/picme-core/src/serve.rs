//! Serveur HTTP de la carte (touche G) : une seule page, servie sur `127.0.0.1` le temps de la consultation. Une page
//! `file://` n'envoie pas de Referer et les serveurs de tuiles OpenStreetMap refusent alors toute requete (403) ; en
//! HTTP le navigateur envoie `http://127.0.0.1:<port>/`. Le port est ephemere et l'ecoute n'est jamais ouverte au
//! reseau. Seul `GET /` repond, avec la page tenue en memoire : aucune requete ne touche au disque. L'en-tete `Host`
//! est controle (`127.0.0.1:<port>` ou `localhost:<port>`) pour qu'une page tierce ne puisse pas lire la carte par
//! DNS rebinding. Le serveur s'arrete seul apres `IDLE` sans consultation, ou quand le `Server` est lache.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Delai sans consultation au bout duquel le serveur s'arrete.
pub const IDLE: Duration = Duration::from_secs(600);
/// Taille maximale de la requete (ligne et en-tetes) : au-dela, 431.
const REQUEST_MAX: usize = 8 * 1024;
/// Delai total pour recevoir la requete, et pour ecrire la reponse : un client lent est coupe, il ne bloque personne.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Connexions traitees en meme temps (un navigateur en ouvre quelques-unes d'avance, qui restent muettes).
const MAX_CONN: usize = 16;
/// Pas de la boucle d'ecoute : arret et delai d'inactivite sont verifies a chaque tour.
const POLL: Duration = Duration::from_millis(20);

struct Shared {
    page: String,
    /// Les deux valeurs acceptees de l'en-tete `Host`, en minuscules.
    hosts: [String; 2],
    /// Derniere page servie (ou demarrage).
    last: Mutex<Instant>,
    active: AtomicUsize,
}

/// Le serveur et son thread d'ecoute. Lacher le `Server` l'arrete (dans les `POLL` qui suivent).
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Sert `page` sur un port ephemere de `127.0.0.1`, `IDLE` apres le dernier acces.
    pub fn start(page: String) -> io::Result<Server> {
        Server::start_with(page, IDLE)
    }

    /// Comme `start`, avec un delai d'inactivite choisi (les tests).
    pub fn start_with(page: String, idle: Duration) -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            page,
            hosts: [format!("127.0.0.1:{}", addr.port()), format!("localhost:{}", addr.port())],
            last: Mutex::new(Instant::now()),
            active: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::Builder::new().name("picme-serve".into()).spawn(move || run(&listener, &shared, &stop, idle))?
        };
        Ok(Server { addr, stop, thread: Some(thread) })
    }

    /// Adresse d'ecoute.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// L'adresse a ouvrir dans le navigateur.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.addr.port())
    }

    /// Le thread d'ecoute a rendu la main (delai ecoule ou arret demande).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Decompte d'une connexion en cours ; rendu au drop, meme si le thread n'a pas pu demarrer.
struct Active(Arc<Shared>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn run(listener: &TcpListener, shared: &Arc<Shared>, stop: &AtomicBool, idle: Duration) {
    while !stop.load(Ordering::Relaxed) && shared.last.lock().is_ok_and(|t| t.elapsed() < idle) {
        match listener.accept() {
            Ok((stream, _)) => {
                if shared.active.fetch_add(1, Ordering::Relaxed) >= MAX_CONN {
                    shared.active.fetch_sub(1, Ordering::Relaxed);
                    continue;
                }
                let guard = Active(shared.clone());
                let _ = thread::Builder::new().name("picme-serve-conn".into()).spawn(move || {
                    let shared = guard.0.clone();
                    handle(stream, &shared);
                    drop(guard);
                });
            }
            // rien en attente, ou une erreur passagere (descripteurs epuises) : on reessaie au tour suivant
            Err(_) => thread::sleep(POLL),
        }
    }
}

/// Ce que `read_head` rapporte.
enum Head {
    /// Ligne de requete et en-tetes, sans le `\r\n\r\n` final.
    Done(Vec<u8>),
    /// Requete refusee avec ce code.
    Reject(u16),
    /// Connexion fermee ou en erreur avant toute requete (ouverture speculative d'un navigateur) : pas de reponse.
    Silent,
}

fn read_head(stream: &mut TcpStream) -> Head {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return Head::Reject(408) };
        if stream.set_read_timeout(Some(left)).is_err() {
            return Head::Silent;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Head::Silent,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Head::Reject(408),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Head::Silent,
        }
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            if end > REQUEST_MAX {
                return Head::Reject(431);
            }
            buf.truncate(end);
            return Head::Done(buf);
        }
        if buf.len() > REQUEST_MAX {
            return Head::Reject(431);
        }
    }
}

/// Code de refus d'une requete, ou `None` : la page est due. Ordre : syntaxe (400), `Host` (403), methode (405),
/// chemin (404).
fn check(head: &[u8], shared: &Shared) -> Option<u16> {
    let Ok(text) = std::str::from_utf8(head) else { return Some(400) };
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Some(400) };
    if !version.starts_with("HTTP/1.") {
        return Some(400);
    }
    let mut host = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { return Some(400) };
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim().to_ascii_lowercase()).is_some() {
            return Some(403);
        }
    }
    if !host.is_some_and(|h| shared.hosts.contains(&h)) {
        return Some(403);
    }
    if method != "GET" {
        return Some(405);
    }
    if target != "/" {
        return Some(404);
    }
    None
}

fn handle(mut stream: TcpStream, shared: &Shared) {
    // les sockets acceptees d'une ecoute non bloquante ne le sont pas partout : on le fixe
    if stream.set_nonblocking(false).is_err() || stream.set_write_timeout(Some(REQUEST_TIMEOUT)).is_err() {
        return;
    }
    let status = match read_head(&mut stream) {
        Head::Done(head) => check(&head, shared),
        Head::Reject(code) => Some(code),
        Head::Silent => return,
    };
    let reply = match status {
        None => {
            if let Ok(mut last) = shared.last.lock() {
                *last = Instant::now();
            }
            response(200, "text/html; charset=utf-8", &shared.page)
        }
        Some(code) => response(code, "text/plain; charset=utf-8", &format!("{code} {}\n", reason(code))),
    };
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

fn response(code: u16, content_type: &str, body: &str) -> String {
    let allow = if code == 405 { "Allow: GET\r\n" } else { "" };
    format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\
         Referrer-Policy: strict-origin-when-cross-origin\r\nX-Content-Type-Options: nosniff\r\n{allow}\r\n{body}",
        reason(code),
        body.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Envoie `raw` tel quel et rend toute la reponse (le serveur ferme la connexion).
    fn exchange(port: u16, raw: &str) -> String {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8(out).unwrap()
    }

    fn get(port: u16, path: &str, host: &str) -> String {
        exchange(port, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: */*\r\n\r\n"))
    }

    fn status(reply: &str) -> &str {
        reply.lines().next().unwrap_or("")
    }

    fn body(reply: &str) -> &str {
        reply.split_once("\r\n\r\n").map_or("", |(_, b)| b)
    }

    fn page_server() -> Server {
        Server::start("<html>carte é</html>".to_string()).unwrap()
    }

    /// Attend (au plus 5 s) que `cond` soit vraie.
    fn wait(cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn binds_loopback_only_on_an_ephemeral_port() {
        let s = page_server();
        assert_eq!(s.addr().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(s.addr().port(), 0);
        assert_eq!(s.url(), format!("http://127.0.0.1:{}/", s.addr().port()));
    }

    #[test]
    fn root_answers_200_with_the_page_and_the_expected_headers() {
        let s = page_server();
        let port = s.addr().port();
        let r = get(port, "/", &format!("127.0.0.1:{port}"));
        assert_eq!(status(&r), "HTTP/1.1 200 OK");
        assert_eq!(body(&r), "<html>carte é</html>");
        let head = r.split_once("\r\n\r\n").unwrap().0;
        // Content-Length en octets, pas en caracteres
        assert!(head.contains(&format!("Content-Length: {}", "<html>carte é</html>".len())), "{head}");
        for h in [
            "Content-Type: text/html; charset=utf-8",
            "Cache-Control: no-store",
            "Connection: close",
            "Referrer-Policy: strict-origin-when-cross-origin",
        ] {
            assert!(head.contains(h), "{h} absent de {head}");
        }
        // `localhost` est accepte aussi, quelle que soit la casse
        assert_eq!(status(&get(port, "/", &format!("localhost:{port}"))), "HTTP/1.1 200 OK");
        assert_eq!(status(&get(port, "/", &format!("LocalHost:{port}"))), "HTTP/1.1 200 OK");
    }

    #[test]
    fn other_paths_are_404_and_other_methods_405() {
        let s = page_server();
        let port = s.addr().port();
        let host = format!("127.0.0.1:{port}");
        for path in ["/index.html", "/etc/passwd", "/../Cargo.toml", "/?x=1", "//", "http://evil.example/"] {
            let r = get(port, path, &host);
            assert_eq!(status(&r), "HTTP/1.1 404 Not Found", "{path}");
            assert!(!body(&r).contains("carte"), "{path}");
        }
        let post = exchange(port, &format!("POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\n\r\nhi"));
        assert_eq!(status(&post), "HTTP/1.1 405 Method Not Allowed");
        assert!(post.contains("Allow: GET") && !body(&post).contains("carte"));
        assert_eq!(status(&exchange(port, &format!("HEAD / HTTP/1.1\r\nHost: {host}\r\n\r\n"))), "HTTP/1.1 405 Method Not Allowed");
        assert_eq!(status(&exchange(port, "garbage\r\n\r\n")), "HTTP/1.1 400 Bad Request");
    }

    #[test]
    fn a_foreign_or_missing_host_is_refused() {
        let s = page_server();
        let port = s.addr().port();
        for host in ["evil.example", &format!("evil.example:{port}"), &format!("127.0.0.1.evil.example:{port}"), "127.0.0.1", &format!("127.0.0.1:{}", port.wrapping_add(1)), "[::1]:80"] {
            let r = get(port, "/", host);
            assert_eq!(status(&r), "HTTP/1.1 403 Forbidden", "{host}");
            assert!(!body(&r).contains("carte"), "{host}");
        }
        assert_eq!(status(&exchange(port, "GET / HTTP/1.1\r\n\r\n")), "HTTP/1.1 403 Forbidden");
        // deux en-tetes Host : refuse
        let two = exchange(port, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.example\r\n\r\n"));
        assert_eq!(status(&two), "HTTP/1.1 403 Forbidden");
    }

    #[test]
    fn serves_several_requests_including_concurrent_ones() {
        let s = page_server();
        let port = s.addr().port();
        for _ in 0..5 {
            assert_eq!(status(&get(port, "/", &format!("127.0.0.1:{port}"))), "HTTP/1.1 200 OK");
        }
        let hs: Vec<_> = (0..8).map(|_| thread::spawn(move || body(&get(port, "/", &format!("127.0.0.1:{port}"))).to_string())).collect();
        for h in hs {
            assert_eq!(h.join().unwrap(), "<html>carte é</html>");
        }
    }

    #[test]
    fn a_silent_or_slow_client_does_not_block_the_others_and_an_oversized_head_is_refused() {
        let s = page_server();
        let port = s.addr().port();
        // connexions ouvertes qui n'envoient rien, comme les ouvertures speculatives d'un navigateur
        let _mute: Vec<_> = (0..4).map(|_| TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap()).collect();
        // un client qui envoie la requete a petits morceaux et ne la finit pas
        let mut slow = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0").unwrap();
        let t = Instant::now();
        assert_eq!(status(&get(port, "/", &format!("127.0.0.1:{port}"))), "HTTP/1.1 200 OK");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        // en-tetes sans fin : 431, sans attendre le delai
        let big = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX: {}\r\n\r\n", "a".repeat(REQUEST_MAX + 100));
        let mut s2 = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s2.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let _ = s2.write_all(big.as_bytes());
        let mut out = Vec::new();
        let _ = s2.read_to_end(&mut out);
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 431"), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn stops_after_its_idle_delay_and_a_served_page_pushes_it_back() {
        let s = Server::start_with("p".to_string(), Duration::from_millis(600)).unwrap();
        let port = s.addr().port();
        assert!(!s.is_finished());
        // une consultation a mi-chemin repousse l'echeance
        thread::sleep(Duration::from_millis(400));
        assert_eq!(status(&get(port, "/", &format!("127.0.0.1:{port}"))), "HTTP/1.1 200 OK");
        thread::sleep(Duration::from_millis(400));
        assert!(!s.is_finished(), "800 ms apres le demarrage mais 400 ms apres la derniere page");
        assert!(wait(|| s.is_finished()), "le serveur doit s'arreter apres son delai");
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(), "le port est ferme");
    }

    #[test]
    fn dropping_the_server_stops_it_and_closes_the_port() {
        let s = page_server();
        let port = s.addr().port();
        assert_eq!(status(&get(port, "/", &format!("127.0.0.1:{port}"))), "HTTP/1.1 200 OK");
        drop(s);
        assert!(wait(|| TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err()), "le port reste ouvert apres le drop");
    }
}
