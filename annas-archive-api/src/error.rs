use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("HTTP error: status {status}")]
    Http { status: u16 },

    #[error("Parse error: {message}")]
    Parse { message: String },

    #[error("API error: {message}")]
    Api { message: String },

    #[error("Missing API key - set ANNAS_ARCHIVE_API_KEY to your account's secret key")]
    MissingApiKey,

    #[error("Blocked by Anna's Archive's browser check")]
    BrowserCheck,

    #[error("Browser error: {message}")]
    Browser { message: String },

    #[error("All domains failed: {message}")]
    AllDomainsFailed { message: String },
}
