use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Input(String),
    #[error("Erro no disco: {0}. Verifique permissões e espaço disponível.")]
    Io(#[from] std::io::Error),
    #[error("Não foi possível conectar ao servidor. Verifique sua conexão.")]
    Connection,
    #[error("O servidor demorou demais para responder. Tente novamente.")]
    Timeout,
    #[error("O servidor respondeu HTTP {0}. O link pode estar indisponível ou exigir acesso no navegador.")]
    Http(u16),
    #[error("{0}")]
    Host(String),
    #[error("{0}")]
    Authorization(String),
    #[error("Download cancelado.")]
    Cancelled,
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else if let Some(status) = error.status() {
            Self::Http(status.as_u16())
        } else if error.is_decode() {
            Self::Host("O site retornou uma resposta inesperada. Abra a página no navegador ou informe um link direto.".into())
        } else {
            // Não registrar URLs assinadas, senhas ou tokens contidos no erro original.
            Self::Connection
        }
    }
}

impl Error {
    pub fn needs_authorization(&self) -> bool {
        matches!(self, Self::Authorization(_) | Self::Http(401 | 403))
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Connection | Self::Timeout | Self::Http(408 | 429 | 500..=599)
        )
    }
}
