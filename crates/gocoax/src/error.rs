use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(String),
    #[error("request timed out")]
    Timeout,
    #[error("authentication failed")]
    Auth,
    #[error("csrf token rejected")]
    Csrf,
    #[error("unexpected http status {0}")]
    HttpStatus(u16),
    #[error("decode {cmd}: {reason}")]
    Decode { cmd: String, reason: String },
    #[error("config error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_messages_render() {
        let e = Error::Decode { cmd: "0x14".into(), reason: "short array".into() };
        assert_eq!(e.to_string(), "decode 0x14: short array");
        assert_eq!(Error::HttpStatus(401).to_string(), "unexpected http status 401");
    }
}
