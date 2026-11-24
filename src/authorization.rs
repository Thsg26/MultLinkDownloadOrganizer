//! Resposta de autenticação vinculada a uma tentativa ativa, nunca à lista inteira.
use std::{
    fmt,
    sync::{Arc, Mutex},
};

use tokio::sync::oneshot;

use crate::{
    browser::{validate_capture, CapturedAccess},
    error::Result,
};

pub enum AuthorizationResponse {
    Granted(CapturedAccess),
    Skip,
}

/// Clones compartilham a mesma resposta de uso único. Não expõe cookies/URLs
/// em Debug, nem permite reutilizar uma captura depois de cancelar a tentativa.
#[derive(Clone)]
pub struct AuthorizationRequest {
    sender: Arc<Mutex<Option<oneshot::Sender<AuthorizationResponse>>>>,
}

impl fmt::Debug for AuthorizationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .field("pending", &self.is_pending())
            .finish()
    }
}

impl AuthorizationRequest {
    pub fn channel() -> (Self, oneshot::Receiver<AuthorizationResponse>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    pub fn is_pending(&self) -> bool {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }

    pub fn grant(&self, access: CapturedAccess) -> Result<bool> {
        validate_capture(&access)?;
        Ok(self.respond(AuthorizationResponse::Granted(access)))
    }

    pub fn skip(&self) -> bool {
        self.respond(AuthorizationResponse::Skip)
    }

    fn respond(&self, response: AuthorizationResponse) -> bool {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .is_some_and(|sender| sender.send(response).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> CapturedAccess {
        CapturedAccess {
            url: "https://cdn.example/authorized".into(),
            referer: "https://vikingfile.com/f/id".into(),
            cookie: "session=secret".into(),
            filename: None,
            user_agent: None,
        }
    }

    #[tokio::test]
    async fn reply_is_once_only_and_cannot_survive_cancellation() {
        let (request, receiver) = AuthorizationRequest::channel();
        let duplicate = request.clone();
        assert!(request.is_pending());
        assert!(request.grant(access()).unwrap());
        assert!(!duplicate.grant(access()).unwrap());
        assert!(!request.is_pending());
        assert!(matches!(
            receiver.await.unwrap(),
            AuthorizationResponse::Granted(_)
        ));
        let (request, receiver) = AuthorizationRequest::channel();
        drop(receiver);
        assert!(!request.is_pending());
        assert!(!request.grant(access()).unwrap());
    }

    #[tokio::test]
    async fn invalid_captures_do_not_consume_pending_reply_or_leak_secrets() {
        let (request, receiver) = AuthorizationRequest::channel();
        let mut invalid = access();
        invalid.url = "file:///C:/private".into();
        assert!(request.grant(invalid).is_err());
        assert!(request.is_pending());
        let debug = format!("{request:?}");
        assert!(!debug.contains("secret") && !debug.contains("https://"));
        assert!(request.skip());
        assert!(matches!(
            receiver.await.unwrap(),
            AuthorizationResponse::Skip
        ));
    }
}
