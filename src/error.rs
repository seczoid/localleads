use thiserror::Error;

#[derive(Debug, Error)]
pub enum LocalLeadsError {
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("proxy entry is invalid: {0}")]
    InvalidProxy(String),
}
