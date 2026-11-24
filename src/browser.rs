//! Navegador auxiliar: mesmo executável, event loop isolado e pipes privados.
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    error::{Error, Result},
    input::validate_http_url,
};

/// Fallback HTTP. A WebView usa seu UA nativo, nunca este valor forçado.
pub const EDGE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36 Edg/157.0.0.0";

// Não implementar Debug: estes dados podem conter URLs assinadas/cookies.
#[derive(Clone, Serialize, Deserialize)]
pub struct CapturedAccess {
    pub url: String,
    pub referer: String,
    pub cookie: String,
    pub filename: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BrowserCommand {
    Open {
        id: usize,
        url: String,
        label: String,
        show: bool,
    },
    Show,
    Minimize,
    Reset,
    Shutdown,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BrowserEvent {
    Ready,
    Captured { id: usize, access: CapturedAccess },
    TabClosed { id: usize },
    Error { id: Option<usize>, message: String },
    Closed,
}

pub struct Browser {
    commands: Sender<BrowserCommand>,
    pub events: Receiver<BrowserEvent>,
    child: Option<Child>,
}

impl Browser {
    pub fn launch() -> Result<Self> {
        if !cfg!(windows) {
            return Err(Error::Host(
                "O navegador integrado desta versão está disponível no Windows com WebView2."
                    .into(),
            ));
        }
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--browser-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(owner) = crate::window_focus::application_window() {
            command.arg("--browser-owner").arg(owner.to_string());
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn()?;
        crate::window_focus::grant(child.id());
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Host("Não foi possível iniciar o canal do navegador.".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Host("Não foi possível ler o canal do navegador.".into()))?;
        let (commands, receiver) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                if serde_json::to_writer(&mut stdin, &command).is_err()
                    || stdin.write_all(b"\n").is_err()
                    || stdin.flush().is_err()
                {
                    break;
                }
                if matches!(command, BrowserCommand::Shutdown) {
                    break;
                }
            }
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(line)) = read_message(&mut reader) {
                if let Ok(event) = serde_json::from_str::<BrowserEvent>(&line) {
                    if events_tx.send(event).is_err() {
                        break;
                    }
                }
            }
            let _ = events_tx.send(BrowserEvent::Closed);
        });
        Ok(Self {
            commands,
            events,
            child: Some(child),
        })
    }

    pub fn send(&self, command: BrowserCommand) -> Result<()> {
        if matches!(
            command,
            BrowserCommand::Show | BrowserCommand::Open { show: true, .. }
        ) {
            if let Some(child) = &self.child {
                crate::window_focus::grant(child.id());
            }
        }
        self.commands
            .send(command)
            .map_err(|_| Error::Host("O navegador foi encerrado. Abra-o novamente.".into()))
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.commands.send(BrowserCommand::Shutdown);
        if let Some(mut child) = self.child.take() {
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

pub(crate) fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let count = reader.take(256 * 1024 + 1).read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if count > 256 * 1024 || !bytes.ends_with(b"\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Mensagem de navegador inválida",
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Mensagem de navegador não UTF-8",
        )
    })
}

pub fn validate_capture(access: &CapturedAccess) -> Result<()> {
    validate_http_url(
        &Url::parse(&access.url).map_err(|_| {
            Error::Host("O navegador retornou uma URL de download inválida.".into())
        })?,
    )?;
    validate_http_url(
        &Url::parse(&access.referer)
            .map_err(|_| Error::Host("A página de origem do download é inválida.".into()))?,
    )?;
    if access.cookie.contains(['\r', '\n'])
        || access.cookie.len() > 32 * 1024
        || reqwest::header::HeaderValue::from_str(&access.cookie).is_err()
    {
        return Err(Error::Host(
            "Cookies inválidos recebidos do navegador.".into(),
        ));
    }
    if access.user_agent.as_ref().is_some_and(|ua| {
        ua.is_empty() || ua.len() > 2048 || reqwest::header::HeaderValue::from_str(ua).is_err()
    }) {
        return Err(Error::Host(
            "User-Agent inválido recebido do navegador.".into(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
#[path = "browser_native.rs"]
mod native;

pub fn run_worker() {
    #[cfg(windows)]
    if let Err(error) = native::run() {
        let event = BrowserEvent::Error {
            id: None,
            message: error.to_string(),
        };
        let mut stdout = std::io::stdout().lock();
        let _ = serde_json::to_writer(&mut stdout, &event);
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_capture_without_local_schemes_or_header_injection() {
        let mut access = CapturedAccess {
            url: "https://cdn.example/file".into(),
            referer: "https://vikingfile.com/f/id".into(),
            cookie: "session=ok".into(),
            filename: None,
            user_agent: None,
        };
        assert!(validate_capture(&access).is_ok());
        access.cookie = "session=ok\r\nX-Injected: yes".into();
        assert!(validate_capture(&access).is_err());
        access.cookie.clear();
        access.url = "file:///C:/secret".into();
        assert!(validate_capture(&access).is_err());
    }
    #[test]
    fn pipe_messages_are_bounded() {
        assert!(read_message(&mut std::io::Cursor::new(b"{}\n"))
            .unwrap()
            .is_some());
        assert!(read_message(&mut std::io::Cursor::new(vec![b'a'; 256 * 1024 + 2])).is_err());
    }
}
