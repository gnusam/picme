//! `picme_core::thumbs::uri` doit rendre exactement l'URI de gio : le file manager nomme ses miniatures par le
//! md5 de `g_file_get_uri`, un octet de difference et la miniature n'est jamais retrouvee.

use gtk4::gio;
use gtk4::gio::prelude::FileExt;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const NAMES: &[&str] = &[
    "/home/sam/Photos/simple.jpg",
    "/home/sam/Photos/with space.jpg",
    "/home/sam/Photos/\u{e9}t\u{e9} \u{e0} la mer.jpg",
    "/home/sam/Photos/hash#1.jpg",
    "/home/sam/Photos/100%.jpg",
    "/home/sam/Photos/%41.jpg",
    "/home/sam/Photos/why?.jpg",
    "/home/sam/Photos/a+b.jpg",
    "/home/sam/Photos/[brackets].jpg",
    "/home/sam/Photos/{braces}|pipe^caret`tick.jpg",
    "/home/sam/Photos/quote'dq\".jpg",
    "/home/sam/Photos/!$&()*,;=:@~-_.jpg",
    "/home/sam/Photos/back\\slash<lt>gt.jpg",
    "/home/sam/Photos/\u{1f4f7}.jpg",
    "/home/sam/Photos/\u{65e5}\u{672c}\u{8a9e}.jpg",
    "/a/b c/d#e/f%g/h?i/j+k/l[m.jpg",
    "/",
];

#[test]
fn uri_matches_gio_on_awkward_names() {
    for name in NAMES {
        let p = Path::new(name);
        assert_eq!(picme_core::thumbs::uri(p), gio::File::for_path(p).uri().as_str(), "{name}");
    }
}

#[test]
fn uri_matches_gio_on_every_ascii_byte() {
    for b in 1u8..128 {
        if b == b'/' {
            continue;
        }
        let name = [b"/x/a".as_slice(), &[b], b"b.jpg"].concat();
        let p = Path::new(OsStr::from_bytes(&name));
        assert_eq!(picme_core::thumbs::uri(p), gio::File::for_path(p).uri().as_str(), "octet {b:#04x}");
    }
}

#[test]
fn uri_matches_gio_on_non_utf8_names() {
    let name = b"/x/caf\xe9 \xff.jpg";
    let p = Path::new(OsStr::from_bytes(name));
    assert_eq!(picme_core::thumbs::uri(p), gio::File::for_path(p).uri().as_str());
}
