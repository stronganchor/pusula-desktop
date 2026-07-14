use std::io;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    User(String),

    #[error("Veritabanı hatası: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Dosya sistemi hatası: {0}")]
    Io(#[from] io::Error),

    #[error("Veri biçimi hatası: {0}")]
    Json(#[from] serde_json::Error),

    #[error("İstek yolu geçersiz: {0}")]
    Url(#[from] url::ParseError),
}

impl AppError {
    pub fn user(message: impl Into<String>) -> Self {
        Self::User(message.into())
    }
}

pub type AppResult<T> = Result<T, AppError>;
