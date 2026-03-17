use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Vault 錯誤：{0}")]
    Vault(String),
    #[error("資料庫錯誤：{0}")]
    Database(String),
    #[error("匯入錯誤：{0}")]
    Import(String),
    #[error("語音錯誤：{0}")]
    Voice(String),
    #[error("AI 錯誤：{0}")]
    AI(String),
    #[error("設定錯誤：{0}")]
    Settings(String),
    #[error("IO 錯誤：{0}")]
    Io(String),
    #[error("安全錯誤：{0}")]
    Security(String),
}

/// 序列化為純字串，讓前端 catch 到的 error 直接是可讀字串
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Import(e.to_string())
    }
}

impl From<url::ParseError> for AppError {
    fn from(e: url::ParseError) -> Self {
        AppError::Security(format!("無效的 URL：{}", e))
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
