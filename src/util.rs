use thiserror::Error;

#[derive(Error, Debug)]
pub enum ShadowError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("http error: {0}")]
    Http(#[from] http::Error),

    #[error("uri parse error: {0}")]
    UriParse(#[from] http::uri::InvalidUri),

    #[error("url parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("invalid header value: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),

    #[error("invalid header name: {0}")]
    InvalidHeaderName(#[from] http::header::InvalidHeaderName),

    #[error("cache error: {0}")]
    Cache(String),

    #[error("tls configuration error: {0}")]
    TlsConfig(String),

    #[error("actix web error: {0}")]
    ActixWeb(#[from] actix_web::Error), // for general actix_web errors

    #[error("actix web payload error: {0}")]
    ActixWebPayload(#[from] actix_web::error::PayloadError),

    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),
}

pub type Result<T> = std::result::Result<T, ShadowError>;

pub fn setup_logger() {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
} 