use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Another `Db` (in this or another process) already has the directory open.
    Locked,
    KeyTooLarge(usize),
    ValueTooLarge(usize),
    /// A previous write to the log, or a flush of the memtable to an SSTable, failed, so
    /// the on-disk state is unknown. The database refuses further writes until it is reopened.
    WalPoisoned,
    /// A file failed its checksum or is malformed.
    Corruption(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Locked => write!(f, "database directory is already open elsewhere"),
            Error::KeyTooLarge(n) => write!(f, "key is {n} bytes, larger than the maximum"),
            Error::ValueTooLarge(n) => write!(f, "value is {n} bytes, larger than the maximum"),
            Error::WalPoisoned => {
                write!(
                    f,
                    "an earlier write to disk failed; reopen the database to recover"
                )
            }
            Error::Corruption(what) => write!(f, "corrupt data: {what}"),
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

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
