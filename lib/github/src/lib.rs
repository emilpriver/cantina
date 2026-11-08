use thiserror::Error;

pub struct GithubClient {}

#[derive(Error, Debug)]
pub enum GithubClientError {
    #[error("InvalidToken: {0}")]
    InvalidToken(String),
}

pub type Result<T> = std::result::Result<T, GithubClientError>;

pub fn new() -> Result<GithubClient> {
    Ok(GithubClient {})
}
