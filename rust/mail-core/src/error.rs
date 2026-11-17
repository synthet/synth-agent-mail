/// Errors from parsing, validating, signing and sealing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A field breaks the schema the TypeScript peer enforces.
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
    /// JSON that does not match the expected shape (unknown field, wrong type, null).
    #[error("malformed JSON: {0}")]
    Json(String),
    /// A key that is not the expected Ed25519 / X25519 PEM.
    #[error("bad key: {0}")]
    Key(String),
    #[error("untrusted sender or invalid signature")]
    Untrusted,
    #[error("wrong recipient or expired packet")]
    WrongRecipientOrExpired,
    #[error("decryption failed")]
    Decrypt,
    #[error("message identity mismatch")]
    IdentityMismatch,
    #[error("invalid message signature")]
    BadSignature,
    #[error("invalid delivery receipt")]
    BadReceipt,
    #[error("identity key mismatch")]
    KeyMismatch,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> Error {
    Error::Invalid {
        field,
        reason: reason.into(),
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error.to_string())
    }
}
