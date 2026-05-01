use gluon_model::SourceSpan;

/// Single diagnostic produced by a builder during script execution.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub message: String,
    pub span: Option<SourceSpan>,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), span: None }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.span {
            Some(s) => write!(f, "{s}: error: {}", self.message),
            None => write!(f, "error: {}", self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

pub type Result<T> = std::result::Result<T, ConfigError>;
