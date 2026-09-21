//! One error type for the whole crate, and the rule that a refusal names
//! both what was asked for and why there is no atomic underneath it.

use std::fmt;

/// Why a call did not answer. See `docs/dist/RUST_API.md` §8.
#[derive(Debug)]
pub enum Error {
    /// The SQL layer refused: a parse error, a Tier-2/Tier-3 construct named
    /// with its tier, or a shape the compiler cannot serve.
    Sql(sekejap_lang::SqlError),
    /// The engine refused or the store failed.
    Engine(sekejap_core::collections::Error),
    /// A bounded walk refused: a work budget, a deadline, a cancel.
    Query(sekejap_core::collections::QueryError),
    /// The service refused: a second writer, a transaction statement through
    /// the guard, a failed publication.
    Service(sekejap_dist::service::ServiceError),
    /// Opening or creating the directory failed.
    Io(std::io::Error),
    /// The named collection is not in the catalog. E4 has no implicit
    /// collection: one document implies no column kinds for the documents
    /// after it.
    UnknownCollection(String),
    /// The named row is not in the collection, on a call that needs it to
    /// exist -- an edge endpoint, for instance.
    UnknownRow { collection: String, key: String },
    /// This crate's own refusal: `construct` is what was asked for, `reason`
    /// is why E4 has no atomic for it. Never an empty answer.
    Refused { construct: String, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// A named refusal. Both halves are required by construction.
    pub fn refused(construct: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Refused {
            construct: construct.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "{e}"),
            Self::Engine(e) => write!(f, "{e}"),
            Self::Query(e) => write!(f, "{e}"),
            Self::Service(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::UnknownCollection(name) => {
                write!(f, "collection `{name}` is not in the catalog")
            }
            Self::UnknownRow { collection, key } => {
                write!(f, "no row `{key}` in collection `{collection}`")
            }
            Self::Refused { construct, reason } => write!(f, "{construct} refused: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<sekejap_lang::SqlError> for Error {
    fn from(e: sekejap_lang::SqlError) -> Self {
        Self::Sql(e)
    }
}
impl From<sekejap_core::collections::Error> for Error {
    fn from(e: sekejap_core::collections::Error) -> Self {
        Self::Engine(e)
    }
}
impl From<sekejap_core::collections::QueryError> for Error {
    fn from(e: sekejap_core::collections::QueryError) -> Self {
        Self::Query(e)
    }
}
impl From<sekejap_dist::service::ServiceError> for Error {
    fn from(e: sekejap_dist::service::ServiceError) -> Self {
        Self::Service(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
