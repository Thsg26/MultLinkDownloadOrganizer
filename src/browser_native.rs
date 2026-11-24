use std::{
    io::{BufReader, Write},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::{run_return::EventLoopExtRunReturn, windows::WindowExtWindows},
    window::{Window, WindowBuilder},
};
use wry::{
    dpi::{LogicalPosition, LogicalSize as WebSize},
    NewWindowResponse, PageLoadEvent, PermissionResponse, Rect, WebContext, WebView,
    WebViewBuilder,
};

use super::{read_message, validate_capture, BrowserCommand, BrowserEvent, CapturedAccess};
use crate::{
    error::{Error, Result},
    input::validate_http_url,
};

const HEADER: f64 = 118.0;

enum Internal {
    Command(BrowserCommand),
    Toolbar(Toolbar),
    Capture {
        id: usize,
        token: u64,
        url: String,
        filename: Option<String>,
    },
    CompleteCapture {
        id: usize,
        token: u64,
        url: String,
        filename: Option<String>,
        user_agent: Option<String>,
    },
    Loaded {
        id: usize,
        token: u64,
    },
}

#[derive(Deserialize)]
struct Toolbar {
    action: String,
    id: Option<usize>,
}

struct Tab {
    id: usize,
    label: String,
    view: WebView,
    captured: bool,
    token: u64,
}

struct State {
    window: Window,
    toolbar: WebView,
    context: WebContext,
    tabs: Vec<Tab>,
    selected: Option<usize>,
    proxy: EventLoopProxy<Internal>,
    next_token: u64,
}

fn browser_error() -> Error {
    Error::Host("Não foi possível abrir o navegador integrado. Verifique a instalação do Microsoft Edge WebView2 Runtime x64 e tente novamente.".into())
}

pub fn run() -> Result<()> {
    let profile = tempfile::Builder::new().prefix("mldo-browser-").tempdir()?;
    let mut event_loop = EventLoopBuilder::<Internal>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("MLDO — Autenticação · Edge/WebView2 · uma página")
        .with_inner_size(LogicalSize::new(1000.0, 760.0))
        .with_min_inner_size(LogicalSize::new(760.0, 560.0))
        .with_visible(false)
        .build(&event_loop)
        .map_err(|_| browser_error())?;
    let args: Vec<_> = std::env::args().collect();
    if let Some(owner) = args
        .windows(2)
        .find(|pair| pair[0] == "--browser-owner")
        .and_then(|pair| pair[1].parse().ok())
    {
        crate::window_focus::set_owner(window.hwnd(), owner);
    }
    let ipc_proxy = proxy.clone();
    let mut toolbar_context = WebContext::new(Some(profile.path().join("toolbar")));
    let toolbar = WebViewBuilder::new_with_web_context(&mut toolbar_context)
        .with_html(include_str!("browser_toolbar.html"))
        .with_bounds(bounds(&window, true))
        .with_navigation_handler(|url| url == "about:blank")
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_download_started_handler(|_, _| false)
        .with_ipc_handler(move |request| {
            // IPC só existe nesta WebView local. A WebView remota não tem handler.
            if request.body().len() < 1024 {
                if let Ok(command) = serde_json::from_str::<Toolbar>(request.body()) {
                    let _ = ipc_proxy.send_event(Internal::Toolbar(command));
                }
            }
        })
        .build_as_child(&window)
        .map_err(|_| browser_error())?;
    let mut state = State {
        window,
        toolbar,
        context: WebContext::new(Some(profile.path().join("profile"))),
        tabs: Vec::new(),
        selected: None,
        proxy: proxy.clone(),
        next_token: 0,
    };
    // Não mostrar janela vazia nem minimizada: o primeiro Open a traz à frente.
    emit(BrowserEvent::Ready);
    thread::spawn(move || {
        let mut stdin = BufReader::new(std::io::stdin());
        while let Ok(Some(line)) = read_message(&mut stdin) {
            let Ok(command) = serde_json::from_str::<BrowserCommand>(&line) else {
                break;
            };
            let shutdown = matches!(command, BrowserCommand::Shutdown);
            if proxy.send_event(Internal::Command(command)).is_err() || shutdown {
                return;
            }
        }
        let _ = proxy.send_event(Internal::Command(BrowserCommand::Shutdown));
    });
    let mut next_inspection = Instant::now() + Duration::from_secs(2);
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(next_inspection);
        match event {
            Event::UserEvent(Internal::Command(BrowserCommand::Shutdown)) => {
                *flow = ControlFlow::Exit;
            }
            Event::UserEvent(Internal::Command(command)) => {
                if let Err(error) = state.command(command) {
                    emit(BrowserEvent::Error {
                        id: None,
                        message: error.to_string(),
                    });
                }
            }
            Event::UserEvent(Internal::Toolbar(command)) => state.toolbar_command(command),
            Event::UserEvent(Internal::Capture {
                id,
                token,
                url,
                filename,
            }) => state.capture(id, token, url, filename),
            Event::UserEvent(Internal::CompleteCapture {
                id,
                token,
                url,
                filename,
                user_agent,
            }) => {
                state.complete_capture(id, token, url, filename, user_agent);
            }
            Event::UserEvent(Internal::Loaded { id, token }) => {
                if state
                    .tabs
                    .iter()
                    .any(|tab| tab.id == id && tab.token == token)
                {
                    state.sync_toolbar();
                    state.inspect(id);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                state.window.set_minimized(true);
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                let _ = state.toolbar.set_bounds(bounds(&state.window, true));
                for tab in &state.tabs {
                    let _ = tab.view.set_bounds(bounds(&state.window, false));
                }
            }
            Event::MainEventsCleared if Instant::now() >= next_inspection => {
                for tab in &state.tabs {
                    if !tab.captured {
                        state.inspect(tab.id);
                    }
                }
                next_inspection = Instant::now() + Duration::from_secs(2);
            }
            _ => {}
        }
    });
    // run_return permite destruir WebViews antes de remover o perfil temporário.
    state.tabs.clear();
    drop(state);
    emit(BrowserEvent::Closed);
    Ok(())
}

impl State {
    fn command(&mut self, command: BrowserCommand) -> Result<()> {
        match command {
            BrowserCommand::Open {
                id,
                url,
                label,
                show,
            } => {
                let parsed = url::Url::parse(&url)
                    .map_err(|_| Error::Host("Página de navegador inválida.".into()))?;
                validate_http_url(&parsed)?;
                if !self.tabs.iter().any(|tab| tab.id == id) {
                    if self.tabs.iter().any(|tab| !tab.captured) {
                        return Err(Error::Host("Conclua ou feche a página atual antes de abrir o próximo link. O navegador aceita somente uma página por vez.".into()));
                    }
                    for previous in self.tabs.drain(..) {
                        emit(BrowserEvent::TabClosed { id: previous.id });
                    }
                    self.next_token += 1;
                    let token = self.next_token;
                    let download_proxy = self.proxy.clone();
                    let load_proxy = self.proxy.clone();
                    let view = WebViewBuilder::new_with_web_context(&mut self.context)
                        // Manter o UA real do WebView2/Edge, sem sobrescrever.
                        .with_bounds(bounds(&self.window, false))
                        .with_url(&url)
                        .with_focused(false)
                        .with_devtools(false)
                        .with_navigation_handler(|url| {
                            url::Url::parse(&url).is_ok_and(|u| validate_http_url(&u).is_ok())
                        })
                        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
                        .with_permission_handler(|_| PermissionResponse::Deny)
                        .with_download_started_handler(move |url, path: &mut PathBuf| {
                            let filename = path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned());
                            let _ = download_proxy.send_event(Internal::Capture {
                                id,
                                token,
                                url,
                                filename,
                            });
                            false // download nativo cancelado: somente a fila do MLDO pode escrever.
                        })
                        .with_on_page_load_handler(move |event, _| {
                            if matches!(event, PageLoadEvent::Finished) {
                                let _ = load_proxy.send_event(Internal::Loaded { id, token });
                            }
                        })
                        .build_as_child(&self.window)
                        .map_err(|_| browser_error())?;
                    self.tabs.push(Tab {
                        id,
                        label,
                        view,
                        captured: false,
                        token,
                    });
                }
                self.select(id);
                if show {
                    self.show();
                }
            }
            BrowserCommand::Show => {
                if !self.tabs.is_empty() {
                    self.show();
                }
            }
            BrowserCommand::Minimize => self.window.set_minimized(true),
            BrowserCommand::Reset => {
                self.tabs.clear();
                self.selected = None;
                self.sync_toolbar();
                self.window.set_visible(false);
            }
            BrowserCommand::Shutdown => {}
        }
        Ok(())
    }

    fn show(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
        crate::window_focus::bring_forward(self.window.hwnd());
        if let Some(tab) = self.tabs.first() {
            let _ = tab.view.focus();
        }
    }

    fn select(&mut self, id: usize) {
        self.selected = Some(id);
        for tab in &self.tabs {
            let _ = tab.view.set_visible(tab.id == id);
        }
        self.sync_toolbar();
    }

    fn toolbar_command(&mut self, command: Toolbar) {
        let id = command.id.or(self.selected);
        match command.action.as_str() {
            "select" => {
                if let Some(id) = id {
                    self.select(id);
                }
            }
            "close" => {
                self.tabs.retain(|tab| Some(tab.id) != id);
                if let Some(id) = id {
                    emit(BrowserEvent::TabClosed { id });
                }
                self.selected = self.tabs.last().map(|tab| tab.id);
                if let Some(id) = self.selected {
                    self.select(id);
                } else {
                    self.sync_toolbar();
                }
            }
            "minimize" => self.window.set_minimized(true),
            "capture" => {
                if let Some(id) = id {
                    self.inspect(id);
                }
            }
            "reload" | "back" => {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| Some(tab.id) == id) {
                    tab.captured = false;
                    if command.action == "reload" {
                        let _ = tab.view.reload();
                    } else {
                        let _ = tab.view.go_back();
                    }
                }
            }
            _ => {}
        }
    }

    fn inspect(&self, id: usize) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id && !tab.captured) else {
            return;
        };
        let proxy = self.proxy.clone();
        let token = tab.token;
        // Apenas leitura de links já liberados. Não clicar ou interagir com captcha.
        let _ = tab.view.evaluate_script_with_callback(r#"(() => {
            const a = document.querySelector('a#download-link[href], a[download][href]');
            if (!a || !/^https?:/.test(a.href) || a.getAttribute('href').startsWith('#')) return null;
            return {url:a.href, filename:a.getAttribute('download') || null};
        })()"#, move |json| {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(url) = value["url"].as_str() {
                    let _ = proxy.send_event(Internal::Capture { id, token, url: url.into(), filename: value["filename"].as_str().map(str::to_owned) });
                }
            }
        });
    }

    fn capture(&mut self, id: usize, token: u64, url: String, filename: Option<String>) {
        let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id && tab.token == token && !tab.captured)
        else {
            return;
        };
        let proxy = self.proxy.clone();
        let _ = tab
            .view
            .evaluate_script_with_callback("navigator.userAgent", move |json| {
                let user_agent = serde_json::from_str::<String>(&json).ok();
                let _ = proxy.send_event(Internal::CompleteCapture {
                    id,
                    token,
                    url: url.clone(),
                    filename: filename.clone(),
                    user_agent,
                });
            });
    }

    fn complete_capture(
        &mut self,
        id: usize,
        token: u64,
        url: String,
        filename: Option<String>,
        user_agent: Option<String>,
    ) {
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id && tab.token == token && !tab.captured)
        else {
            return;
        };
        let referer = tab.view.url().unwrap_or_default();
        let cookie = match tab.view.cookies_for_url(&url) {
            Ok(cookies) => cookies
                .iter()
                .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
                .collect::<Vec<_>>()
                .join("; "),
            Err(_) => {
                emit(BrowserEvent::Error {
                    id: Some(id),
                    message: "Não foi possível compartilhar a sessão. Tente recarregar a página."
                        .into(),
                });
                return;
            }
        };
        let access = CapturedAccess {
            url,
            referer,
            cookie,
            filename,
            user_agent,
        };
        if validate_capture(&access).is_err() {
            return;
        }
        tab.captured = true;
        emit(BrowserEvent::Captured { id, access });
        self.sync_toolbar();
    }

    fn sync_toolbar(&self) {
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .map(|tab| serde_json::json!({"id":tab.id,"label":tab.label,"captured":tab.captured}))
            .collect();
        let address = self
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == self.selected)
            .and_then(|tab| tab.view.url().ok())
            .unwrap_or_default();
        let model = serde_json::json!({"tabs":tabs,"selected":self.selected,"address":address});
        let _ = self
            .toolbar
            .evaluate_script(&format!("window.updateMldo({model})"));
    }
}

fn bounds(window: &Window, toolbar: bool) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    Rect {
        position: LogicalPosition::new(0.0, if toolbar { 0.0 } else { HEADER }).into(),
        size: WebSize::new(
            size.width,
            if toolbar {
                HEADER
            } else {
                (size.height - HEADER).max(1.0)
            },
        )
        .into(),
    }
}

fn emit(event: BrowserEvent) {
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &event);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}
