//! One error type for the whole crate.
//!
//! `Usage` is for anything the user can fix on the command line (exit code 2);
//! everything else is exit code 1.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store error: {0}")]
    Store(String),
    #[error("expression error: {0}")]
    Expr(String),
    #[error("{0}")]
    Usage(String),
    #[error("corrupt meta record: {0}")]
    Cbor(String),
    #[error("import aborted: {0}")]
    Aborted(String),
    /// The user declined a confirmation prompt; nothing was changed.
    #[error("cancelled: {0}")]
    Cancelled(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Io(e.into())
    }
}

impl From<minicbor::decode::Error> for Error {
    fn from(e: minicbor::decode::Error) -> Self {
        Error::Cbor(e.to_string())
    }
}

impl Error {
    pub fn store(e: impl std::fmt::Display) -> Self {
        Error::Store(e.to_string())
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Usage(_) | Error::Expr(_) => 2,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_is_exit_one() {
        let e = Error::Cancelled("nothing deleted".into());
        assert_eq!(e.exit_code(), 1);
        assert_eq!(e.to_string(), "cancelled: nothing deleted");
    }

    #[test]
    fn exit_codes_and_messages() {
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
        assert_eq!(Error::Expr("x".into()).exit_code(), 2);
        assert_eq!(Error::Store("x".into()).exit_code(), 1);
        assert_eq!(Error::store("boom").to_string(), "store error: boom");
        let io: Error = std::io::Error::other("disk").into();
        assert_eq!(io.to_string(), "io error: disk");
        assert_eq!(io.exit_code(), 1);
        let cbor: Error = minicbor::decode::Error::end_of_input().into();
        assert!(cbor.to_string().starts_with("corrupt meta record"));
        assert_eq!(Error::Aborted("x".into()).to_string(), "import aborted: x");
        let js: Error = serde_json::from_str::<u8>("x").unwrap_err().into();
        assert!(js.to_string().starts_with("io error"));
    }
}
