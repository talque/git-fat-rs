/// Common utilities
use std::io;

/// Most functions return an io::Result so this is a helper to wrap arbitrary
/// errors as io::Error with io::ErrorKind::Other.
pub fn other_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}
