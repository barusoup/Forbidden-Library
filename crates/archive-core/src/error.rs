/// Everything the engine can fail with. Callers mostly surface `to_string()`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("image error: {0}")]
    Image(#[from] image::ImageError),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The password does not open this lock. Kept apart from `Msg` so the
    /// interface can ask again instead of reporting a fault.
    #[error("that password does not open this collection")]
    WrongPassword,

    #[error("{0}")]
    Msg(String),
}

impl Error {
    pub fn msg(text: impl Into<String>) -> Self {
        Self::Msg(text.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
