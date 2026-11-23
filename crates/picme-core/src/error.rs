//! Erreur commune de picme-core.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Fichier illisible ou corrompu (message de la lib de decode).
    Decode(String),
    /// Format sans decodeur (RAW, video) ou variante non geree (JPEG CMYK).
    Unsupported(String),
    /// Argument incoherent (ex. miniature plus grande que 256 px).
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Erreur de decode a partir de l'erreur d'une lib.
    pub(crate) fn decode(e: impl fmt::Display) -> Self {
        Error::Decode(e.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Decode(m) => write!(f, "decode: {m}"),
            Error::Unsupported(m) => write!(f, "non gere: {m}"),
            Error::Invalid(m) => write!(f, "invalide: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
