use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};
use mldo::{
    authorization::AuthorizationRequest,
    browser::{Browser, BrowserCommand, BrowserEvent, CapturedAccess},
    download::{self, Event, PartProgress, Request, Status},
    input::{self, Link, Structure},
    mapping::{self, Assignment, Job},
    staging::PendingDownload,
};
use tokio_util::sync::CancellationToken;

const BG: Color32 = Color32::from_rgb(13, 17, 25);
const SIDEBAR: Color32 = Color32::from_rgb(18, 24, 34);
const CARD: Color32 = Color32::from_rgb(24, 32, 44);
const BORDER: Color32 = Color32::from_rgb(42, 53, 68);
const MUTED: Color32 = Color32::from_rgb(143, 159, 180);
const TEXT: Color32 = Color32::from_rgb(232, 239, 249);
const ACCENT: Color32 = Color32::from_rgb(81, 218, 184);
const AMBER: Color32 = Color32::from_rgb(247, 195, 111);
const RED: Color32 = Color32::from_rgb(246, 130, 143);
const SIDE_WIDTH: f32 = 326.0;

struct Row {
    id: usize,
    path: PathBuf,
    status: Status,
    downloaded: u64,
    total: Option<u64>,
    speed: f64,
    parts: Vec<PartProgress>,
    pending: Option<PendingDownload>,
}

impl Row {
    fn queued(id: usize, path: PathBuf) -> Self {
        Self {
            id,
            path,
            status: Status::Queued,
            downloaded: 0,
            total: None,
            speed: 0.0,
            parts: Vec::new(),
            pending: None,
        }
    }

    fn reset_progress(&mut self) {
        self.downloaded = 0;
        self.total = None;
        self.speed = 0.0;
        self.parts.clear();
    }
}

struct WorkspaceLayout {
    setup: egui::Rect,
    activity: egui::Rect,
    center: egui::Rect,
    cards: Vec<(usize, egui::Rect)>,
}

struct PendingAuthorization {
    request: AuthorizationRequest,
    automatic: bool,
    requested_by_user: bool,
}

pub struct App {
    links_path: Option<PathBuf>,
    structure_path: Option<PathBuf>,
    destination: Option<PathBuf>,
    links: Vec<Link>,
    structure: Option<Structure>,
    assignments: Vec<Assignment>,
    parallel: usize,
    multiple_connections: bool,
    connections: usize,
    rows: Vec<Row>,
    logs: VecDeque<String>,
    session: Instant,
    receiver: Option<Receiver<Event>>,
    cancellation: Option<CancellationToken>,
    running: bool,
    closing: bool,
    review_open: bool,
    only_pending: bool,
    search: String,
    notice: Option<(String, bool)>,
    browser: Option<Browser>,
    browser_ready: bool,
    browser_opened: HashSet<usize>,
    browser_queue: VecDeque<usize>,
    authorizations: HashMap<usize, PendingAuthorization>,
    browser_auto_paused: bool,
    browser_auto: bool,
    browser_window_open: bool,
    browser_only_pending: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let app = Self::with_context(&cc.egui_ctx);
        #[cfg(debug_assertions)]
        let app = debug_preview(app);
        app
    }

    fn with_context(ctx: &egui::Context) -> Self {
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = SIDEBAR;
        style.visuals.extreme_bg_color = BG;
        style.visuals.faint_bg_color = CARD;
        style.visuals.override_text_color = Some(TEXT);
        style.visuals.selection.bg_fill = Color32::from_rgb(34, 93, 86);
        style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
        style.visuals.widgets.inactive.bg_fill = CARD;
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(38, 57, 69);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(35, 78, 76);
        style.spacing.item_spacing = Vec2::new(10.0, 10.0);
        style.spacing.button_padding = Vec2::new(14.0, 9.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(12.0));
        ctx.set_style(style);
        let mut app = Self {
            links_path: None,
            structure_path: None,
            destination: None,
            links: Vec::new(),
            structure: None,
            assignments: Vec::new(),
            parallel: 3,
            multiple_connections: false,
            connections: 3,
            rows: Vec::new(),
            logs: VecDeque::new(),
            session: Instant::now(),
            receiver: None,
            cancellation: None,
            running: false,
            closing: false,
            review_open: false,
            only_pending: true,
            search: String::new(),
            notice: None,
            browser: None,
            browser_ready: false,
            browser_opened: HashSet::new(),
            browser_queue: VecDeque::new(),
            authorizations: HashMap::new(),
            browser_auto_paused: false,
            browser_auto: true,
            browser_window_open: false,
            browser_only_pending: true,
        };
        app.log("Pronto. Selecione os arquivos de entrada e a pasta-base/disco.");
        app
    }

    fn log(&mut self, message: impl AsRef<str>) {
        let seconds = self.session.elapsed().as_secs();
        let message = message.as_ref().replace(['\n', '\r'], " ");
        self.logs.push_back(format!(
            "[{:02}:{:02}:{:02}] {message}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        ));
        while self.logs.len() > 1500 {
            self.logs.pop_front();
        }
    }

    fn error(&mut self, message: impl ToString) {
        let message = message.to_string();
        self.log(&message);
        self.notice = Some((message, true));
    }

    fn reset_mapping(&mut self) {
        self.authorizations.clear();
        self.browser_auto_paused = false;
        self.browser = None;
        self.browser_ready = false;
        self.browser_opened.clear();
        self.browser_queue.clear();
        self.assignments = self
            .structure
            .as_ref()
            .map(|s| mapping::suggest(&self.links, s))
            .unwrap_or_default();
        self.rows.clear();
        self.notice = None;
    }

    fn pick_links(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Selecionar Links.txt")
            .add_filter("Arquivo de texto", &["txt"])
            .pick_file()
        {
            self.links.clear();
            self.links_path = None;
            self.reset_mapping();
            match input::read_text(&path).and_then(|text| input::parse_links(&text)) {
                Ok(links) => {
                    self.log(format!("{} links encontrados.", links.len()));
                    self.links = links;
                    self.links_path = Some(path);
                    self.reset_mapping();
                }
                Err(error) => self.error(error),
            }
        }
    }

    fn pick_structure(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Selecionar download_schema.json ou estrutura antiga")
            .add_filter("Schema JSON ou texto", &["json", "txt"])
            .pick_file()
        {
            self.structure = None;
            self.structure_path = None;
            self.reset_mapping();
            match input::read_text(&path).and_then(|text| input::parse_structure(&text)) {
                Ok(structure) => {
                    self.log(format!(
                        "Estrutura carregada: {} arquivos e {} pastas.",
                        structure.files.len(),
                        structure.directories.len()
                    ));
                    if structure.detailed {
                        self.log("Schema detalhado carregado. A identificação usará o arquivo completo e seu tamanho final; nenhuma associação é exigida antes de baixar.");
                    }
                    self.structure = Some(structure);
                    self.structure_path = Some(path);
                    self.reset_mapping();
                }
                Err(error) => self.error(error),
            }
        }
    }

    fn jobs(&self) -> mldo::error::Result<Vec<Job>> {
        let structure = self
            .structure
            .as_ref()
            .ok_or_else(|| mldo::error::Error::Input("Selecione o arquivo de estrutura.".into()))?;
        mapping::build_jobs(&self.links, structure, &self.assignments)
    }

    fn can_start(&self) -> bool {
        let has_downloads = self.rows.is_empty()
            || self.rows.iter().any(|row| {
                matches!(
                    row.status,
                    Status::Queued | Status::Failed(_) | Status::Cancelled
                )
            });
        !self.running
            && has_downloads
            && !self.links.is_empty()
            && self.destination.is_some()
            && self.jobs().is_ok()
    }

    fn connections_per_file(&self) -> usize {
        if self.multiple_connections {
            self.connections
        } else {
            1
        }
    }

    fn start(&mut self, retry: bool) {
        let Some(destination) = self.destination.clone() else {
            return;
        };
        let Some(structure) = self.structure.clone() else {
            return;
        };
        let mut jobs = match self.jobs() {
            Ok(jobs) => jobs,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if retry || !self.rows.is_empty() {
            jobs.retain(|job| {
                self.rows.iter().any(|r| {
                    r.id == job.id
                        && (matches!(r.status, Status::Failed(_) | Status::Cancelled)
                            || (!retry && r.status == Status::Queued))
                })
            });
            for job in &jobs {
                if let Some(row) = self.rows.iter_mut().find(|r| r.id == job.id) {
                    row.status = Status::Queued;
                    row.reset_progress();
                }
            }
        } else {
            self.rows = jobs
                .iter()
                .map(|job| Row::queued(job.id, PathBuf::from(job.label())))
                .collect();
        }
        if jobs.is_empty() {
            return;
        }
        self.clear_authorizations();
        self.browser = None; // não reutilizar eventos de uma fila anterior
        self.browser_ready = false;
        self.browser_auto_paused = false;
        let (tx, rx) = mpsc::channel();
        match download::start(
            Request {
                destination,
                structure,
                jobs,
                parallel: self.parallel,
                connections: self.connections_per_file(),
                interactive_auth: true,
            },
            tx,
        ) {
            Ok(cancellation) => {
                self.receiver = Some(rx);
                self.cancellation = Some(cancellation);
                self.running = true;
                self.notice = None;
                self.log("Preparando destino e iniciando fila…");
            }
            Err(error) => self.error(error),
        }
    }

    fn receive(&mut self) {
        // Limitar trabalho por frame mesmo em listas grandes; o progresso é amostrado no worker.
        let events: Vec<_> = self
            .receiver
            .as_ref()
            .map(|rx| rx.try_iter().take(1000).collect())
            .unwrap_or_default();
        for event in events {
            match event {
                Event::AuthorizationRequired {
                    id,
                    request,
                    automatic,
                } => {
                    if self.auth_scope_active()
                        && request.is_pending()
                        && self
                            .rows
                            .iter()
                            .any(|row| row.id == id && row.status == Status::AwaitingAuthorization)
                    {
                        self.authorizations.insert(
                            id,
                            PendingAuthorization {
                                request,
                                automatic,
                                requested_by_user: false,
                            },
                        );
                        if automatic && !self.browser_queue.contains(&id) {
                            self.browser_queue.push_back(id);
                        }
                        if !automatic {
                            self.log(format!("Linha {}: o servidor voltou a exigir acesso. A vaga continua reservada; abra o navegador manualmente ou pule o arquivo.", id + 1));
                        }
                    }
                }
                Event::State { id, status } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        if status == Status::Resolving {
                            row.reset_progress();
                        }
                        row.status = status;
                        if row.status != Status::Downloading {
                            row.speed = 0.0;
                        }
                    }
                }
                Event::Progress {
                    id,
                    downloaded,
                    total,
                    speed,
                } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        row.downloaded = downloaded;
                        row.total = total;
                        row.speed = speed;
                    }
                }
                Event::PartsProgress { id, parts } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        let downloaded = parts.iter().map(|part| part.downloaded).sum();
                        if row.parts.len() != parts.len() || downloaded < row.downloaded {
                            row.speed = 0.0;
                        }
                        row.downloaded = downloaded;
                        row.total = parts.iter().map(|part| part.total).sum();
                        row.parts = parts;
                    }
                }
                Event::Identified { id, filename } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        row.path = PathBuf::from(filename);
                    }
                }
                Event::Organized { id, relative } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        row.path = relative.clone();
                        row.pending = None;
                    }
                    if let (Some(structure), Some(assignment)) =
                        (&self.structure, self.assignments.get_mut(id))
                    {
                        assignment.target = structure
                            .files
                            .iter()
                            .position(|path| input::path_key(path) == input::path_key(&relative));
                        assignment.basis = "Organizado após concluir o download".into();
                    }
                }
                Event::Unidentified {
                    id,
                    relative,
                    reason,
                } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        row.path = relative;
                        row.pending = None;
                    }
                    if let Some(assignment) = self.assignments.get_mut(id) {
                        assignment.target = None;
                        assignment.basis = reason;
                    }
                }
                Event::Pending { id, download } => {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                        row.pending = Some(download.clone());
                        row.status = Status::PendingOrganization;
                        row.downloaded = download.bytes;
                        row.total = Some(download.bytes);
                        row.speed = 0.0;
                    }
                    self.log(format!(
                        "Linha {}: download completo em {}. {}",
                        id + 1,
                        download.relative.display(),
                        download.reason
                    ));
                }
                Event::Log(message) => self.log(message),
                Event::Fatal(message) => {
                    self.clear_authorizations();
                    for row in &mut self.rows {
                        if row.status == Status::Queued || row.status.active() {
                            row.status = Status::Failed(message.clone());
                            row.speed = 0.0;
                        }
                    }
                    self.error(message);
                }
                Event::Finished => {
                    self.clear_authorizations();
                    self.running = false;
                    self.cancellation = None;
                    let completed = self
                        .rows
                        .iter()
                        .filter(|r| r.status == Status::Completed)
                        .count();
                    let failed = self
                        .rows
                        .iter()
                        .filter(|r| matches!(r.status, Status::Failed(_)))
                        .count();
                    let cancelled = self
                        .rows
                        .iter()
                        .filter(|r| r.status == Status::Cancelled)
                        .count();
                    let pending = self
                        .rows
                        .iter()
                        .filter(|row| row.status == Status::PendingOrganization)
                        .count();
                    let unidentified = self
                        .rows
                        .iter()
                        .filter(|row| row.status == Status::NotIdentified)
                        .count();
                    self.log(format!("Fila encerrada: {completed} organizados, {unidentified} em NotIdentify, {pending} completos com falha de movimentação, {failed} com erro, {cancelled} cancelados."));
                    if self.notice.is_none() {
                        self.notice = Some((
                            if failed > 0 {
                                "Alguns arquivos falharam. Consulte o log; você pode tentar somente as pendências.".into()
                            } else if pending > 0 {
                                format!("{pending} completos preservados no temporário por falha de movimentação. Corrija as permissões/espaço e use Reprocessar completos, sem baixar novamente.")
                            } else if unidentified > 0 {
                                format!("{completed} organizados; {unidentified} salvos em NotIdentify, fora da pasta principal.")
                            } else if cancelled > 0 {
                                "Fila cancelada. Arquivos concluídos foram preservados.".into()
                            } else {
                                "Tudo pronto! Os arquivos foram salvos e organizados no destino."
                                    .into()
                            },
                            failed > 0,
                        ));
                    }
                }
            }
        }
    }

    fn auth_scope_active(&self) -> bool {
        self.running
            && !self.closing
            && self
                .cancellation
                .as_ref()
                .is_none_or(|cancel| !cancel.is_cancelled())
    }

    fn can_authorize(&self, id: usize) -> bool {
        self.auth_scope_active()
            && self
                .authorizations
                .get(&id)
                .is_some_and(|pending| pending.request.is_pending())
            && self
                .rows
                .iter()
                .any(|row| row.id == id && row.status == Status::AwaitingAuthorization)
    }

    fn next_authorization(&mut self) -> Option<usize> {
        if !self.auth_scope_active() || self.browser_auto_paused || !self.browser_opened.is_empty()
        {
            return None;
        }
        for _ in 0..self.browser_queue.len() {
            let id = self.browser_queue.pop_front()?;
            if !self.can_authorize(id) {
                continue;
            }
            if self.authorizations.get(&id).is_some_and(|pending| {
                pending.requested_by_user || (self.browser_auto && pending.automatic)
            }) {
                return Some(id);
            }
            self.browser_queue.push_back(id);
        }
        None
    }

    fn clear_authorizations(&mut self) {
        self.authorizations.clear();
        self.browser_queue.clear();
        self.browser_opened.clear();
        if let Some(browser) = &self.browser {
            let _ = browser.send(BrowserCommand::Reset);
        }
    }

    fn retire_browser_page(&mut self, id: usize) {
        if self.browser_opened.remove(&id) {
            if let Some(browser) = &self.browser {
                let _ = browser.send(BrowserCommand::Reset);
            }
        }
    }

    fn grant_authorization(&mut self, id: usize, access: CapturedAccess) -> bool {
        if !self.can_authorize(id) {
            return false;
        }
        let result = self.authorizations[&id].request.grant(access.clone());
        match result {
            Ok(true) => {
                self.authorizations.remove(&id);
                self.browser_queue.retain(|queued| *queued != id);
                self.retire_browser_page(id);
                if let Some(assignment) = self.assignments.get_mut(id) {
                    assignment.direct_url = access.url.clone();
                    assignment.browser_access = Some(access);
                }
                self.log(format!("Linha {}: acesso liberado. Continuando automaticamente o mesmo download na vaga reservada.", id + 1));
                true
            }
            Ok(false) => false,
            Err(error) => {
                self.error(error);
                false
            }
        }
    }

    fn skip_authorization(&mut self, id: usize) {
        if !self.can_authorize(id) {
            return;
        }
        if let Some(pending) = self.authorizations.remove(&id) {
            pending.request.skip();
        }
        self.browser_queue.retain(|queued| *queued != id);
        self.retire_browser_page(id);
        self.log(format!(
            "Linha {} pulada pelo usuário; liberando uma vaga da fila.",
            id + 1
        ));
    }

    fn cancel_downloads(&mut self) {
        if let Some(cancel) = &self.cancellation {
            cancel.cancel();
        }
        self.clear_authorizations();
    }

    fn open_browser(&mut self, id: usize, show: bool) {
        if !self.can_authorize(id) {
            return;
        }
        self.browser_auto_paused = false;
        if self.browser_opened.contains(&id) {
            if show {
                if let Some(browser) = &self.browser {
                    let _ = browser.send(BrowserCommand::Show);
                }
            }
            return;
        }
        if !self.browser_opened.is_empty() && !self.browser_opened.contains(&id) {
            if let Some(pending) = self.authorizations.get_mut(&id) {
                pending.requested_by_user = true;
            }
            if !self.browser_queue.contains(&id) {
                self.browser_queue.push_back(id);
                self.log(format!("Linha {} entrou na fila de autenticação. Somente uma página será aberta por vez.", id + 1));
            }
            if show {
                if let Some(browser) = &self.browser {
                    let _ = browser.send(BrowserCommand::Show);
                }
            }
            return;
        }
        let Some(link) = self.links.get(id) else {
            return;
        };
        let url = link.url.to_string();
        let label = format!(
            "L{} · {}",
            link.line,
            link.hint.as_deref().unwrap_or("Arquivo")
        );
        if self.browser.is_none() {
            match Browser::launch() {
                Ok(browser) => {
                    self.browser = Some(browser);
                    self.browser_ready = false;
                }
                Err(error) => {
                    self.browser_auto_paused = true;
                    self.error(error);
                    return;
                }
            }
        }
        if let Some(browser) = &self.browser {
            if let Err(error) = browser.send(BrowserCommand::Reset) {
                self.browser_auto_paused = true;
                self.error(error);
                return;
            }
            match browser.send(BrowserCommand::Open {
                id,
                url,
                label,
                show,
            }) {
                Ok(()) => {
                    self.browser_opened.insert(id);
                }
                Err(error) => {
                    self.browser_auto_paused = true;
                    self.error(error);
                }
            }
        }
    }

    fn receive_auxiliary(&mut self) {
        let events: Vec<_> = self
            .browser
            .as_ref()
            .map(|browser| browser.events.try_iter().take(100).collect())
            .unwrap_or_default();
        for event in events {
            match event {
                BrowserEvent::Ready => {
                    self.browser_ready = true;
                    self.log("Navegador pronto: agente nativo Edge/WebView2, uma página por vez e abertura em primeiro plano.");
                }
                BrowserEvent::Captured { id, access } => {
                    if self.browser_opened.contains(&id) {
                        self.grant_authorization(id, access);
                    }
                }
                BrowserEvent::TabClosed { id } => {
                    if self.browser_opened.contains(&id) {
                        self.skip_authorization(id);
                    }
                }
                BrowserEvent::Error { id, message } => {
                    if id.is_some_and(|id| !self.browser_opened.contains(&id)) {
                        continue;
                    }
                    self.browser_auto_paused = true;
                    let opened: Vec<_> = self.browser_opened.iter().copied().collect();
                    for id in opened {
                        self.retire_browser_page(id);
                    }
                    self.error(message);
                }
                BrowserEvent::Closed => {
                    if self.browser.take().is_some() {
                        self.log("Navegador encerrado. Você pode reabri-lo quando precisar.");
                    }
                    self.browser_ready = false;
                    self.browser_opened.clear();
                    self.browser_auto_paused = true;
                }
            }
        }
        self.authorizations
            .retain(|_, pending| pending.request.is_pending());
        if !self.auth_scope_active() {
            if !self.authorizations.is_empty()
                || !self.browser_opened.is_empty()
                || !self.browser_queue.is_empty()
            {
                self.clear_authorizations();
            }
            return;
        }
        if let Some(id) = self.next_authorization() {
            self.open_browser(id, true);
        }
    }

    fn organize_ready(&mut self) {
        let (Some(destination), Some(structure)) =
            (self.destination.clone(), self.structure.clone())
        else {
            return;
        };
        let entries: Vec<_> = self
            .rows
            .iter()
            .filter_map(|row| row.pending.clone())
            .collect();
        if entries.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        match download::organize_downloads(destination, structure, entries, tx) {
            Ok(cancel) => {
                self.receiver = Some(rx);
                self.cancellation = Some(cancel);
                self.running = true;
                self.notice = None;
                self.log("Organizando downloads completos, sem acessar a rede…");
            }
            Err(error) => self.error(error),
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("NOVA FILA").size(11.0).color(ACCENT).strong());
        ui.add_space(2.0);
        ui.label(RichText::new("Prepare seus downloads").size(21.0).strong());
        ui.label(
            RichText::new("Três escolhas. Tudo no lugar certo.")
                .small()
                .color(MUTED),
        );
        ui.add_space(16.0);
        ui.add_enabled_ui(!self.running, |ui| {
            step(
                ui,
                "01",
                "Lista de links",
                "Um link por linha no arquivo .txt",
            );
            if file_button(ui, self.links_path.as_ref(), "Selecionar Links.txt").clicked() {
                self.pick_links();
            }
            if !self.links.is_empty() {
                ui.label(
                    RichText::new(format!("{} links encontrados", self.links.len()))
                        .color(ACCENT)
                        .small(),
                );
            }
            ui.add_space(10.0);
            step(
                ui,
                "02",
                "Schema / estrutura",
                "JSON com tipo e tamanho exato em bytes",
            );
            if file_button(
                ui,
                self.structure_path.as_ref(),
                "Selecionar download_schema.json",
            )
            .clicked()
            {
                self.pick_structure();
            }
            if let Some(structure) = &self.structure {
                ui.label(
                    RichText::new(format!(
                        "{} arquivos · {} pastas",
                        structure.files.len(),
                        structure.directories.len()
                    ))
                    .small()
                    .color(MUTED),
                );
            }
            ui.add_space(10.0);
            step(
                ui,
                "03",
                "Pasta-base / disco",
                "Temporário, pasta principal e NotIdentify aqui",
            );
            if file_button(ui, self.destination.as_ref(), "Escolher pasta-base / disco").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Onde salvar os downloads?")
                    .pick_folder()
                {
                    self.destination = Some(path);
                    self.rows.clear();
                    self.notice = None;
                }
            }
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(5.0);
            ui.label(RichText::new("Downloads simultâneos").strong());
            ui.horizontal(|ui| {
                for n in 1..=5 {
                    ui.selectable_value(&mut self.parallel, n, format!(" {n} "));
                }
            });
            ui.label(
                RichText::new("O limite vale para a fila inteira.")
                    .small()
                    .color(MUTED),
            );
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.multiple_connections, "Conexões por arquivo");
                ui.add_enabled_ui(self.multiple_connections, |ui| {
                    ui.spacing_mut().button_padding = Vec2::new(3.0, 5.0);
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for n in 1..=5 {
                        ui.selectable_value(&mut self.connections, n, n.to_string());
                    }
                });
            });
            let explanation = if self.multiple_connections {
                format!(
                    "Até {} transferências de partes. Sem suporte seguro, usa 1 conexão.",
                    self.parallel * self.connections_per_file()
                )
            } else {
                "Desativado: uma conexão por arquivo.".into()
            };
            ui.label(RichText::new(explanation).small().color(MUTED));
        });
        ui.add_space(14.0);
        if let Some(structure) = &self.structure {
            if !self.links.is_empty() {
                let equal = self.links.len() == structure.files.len();
                ui.label(
                    RichText::new(if equal {
                        format!(
                            "Quantidades iguais · {} = {}",
                            self.links.len(),
                            structure.files.len()
                        )
                    } else {
                        format!(
                            "Quantidades diferentes · {} ≠ {}",
                            self.links.len(),
                            structure.files.len()
                        )
                    })
                    .small()
                    .color(if equal { ACCENT } else { AMBER }),
                );
                if ui
                    .add_enabled(
                        !self.running,
                        egui::Button::new("Acessos / URLs diretas opcionais"),
                    )
                    .clicked()
                {
                    self.review_open = true;
                }
            }
        }
        if !self.links.is_empty() {
            ui.label(
                RichText::new("Todos os arquivos: baixar no temporário, identificar e mover automaticamente. Não identificados: NotIdentify.")
                .small()
                .color(ACCENT),
            );
        }
        if self.rows.iter().any(|row| row.pending.is_some())
            && ui
                .add_enabled(
                    !self.running,
                    egui::Button::new("Reprocessar completos (sem rede)"),
                )
                .clicked()
        {
            self.organize_ready();
        }
        ui.add_space(10.0);
        if self.running {
            let cancelling = self.cancellation.as_ref().is_some_and(|c| c.is_cancelled());
            if ui
                .add_enabled(
                    !cancelling,
                    egui::Button::new(if cancelling {
                        "Cancelando…"
                    } else {
                        "Cancelar downloads"
                    })
                    .min_size(Vec2::new(ui.available_width(), 44.0)),
                )
                .clicked()
            {
                self.cancel_downloads();
                self.log("Cancelamento solicitado. Finalizando operações e limpando temporários…");
            }
        } else {
            let ready = self.can_start();
            let retry = self
                .rows
                .iter()
                .any(|r| matches!(r.status, Status::Failed(_) | Status::Cancelled));
            if ui
                .add_enabled(
                    ready,
                    egui::Button::new(RichText::new("Iniciar downloads").strong().color(BG))
                        .fill(ACCENT)
                        .min_size(Vec2::new(ui.available_width(), 44.0)),
                )
                .clicked()
            {
                self.start(false);
            }
            if retry
                && ui
                    .add_enabled(
                        ready,
                        egui::Button::new("Tentar somente pendências")
                            .min_size(Vec2::new(ui.available_width(), 34.0)),
                    )
                    .clicked()
            {
                self.start(true);
            }
            if !ready {
                let reason = if self.links.is_empty() || self.structure.is_none() {
                    "Selecione os dois arquivos de entrada.".to_owned()
                } else if self.destination.is_none() {
                    "Escolha uma pasta de destino.".to_owned()
                } else {
                    self.jobs().err().map(|e| e.to_string()).unwrap_or_default()
                };
                ui.label(RichText::new(reason).small().color(MUTED));
            }
        }
        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new("COMPATÍVEL COM").size(10.0).color(MUTED));
        ui.label(RichText::new("VikingFile · Gofile · Buzzheavier").size(12.0));
        ui.checkbox(
            &mut self.browser_auto,
            "Preparar navegador ao exigir captcha/login",
        );
        if ui
            .button(format!(
                "Navegador · {} aguardando",
                self.browser_opened.len()
            ))
            .clicked()
        {
            self.browser_window_open = true;
        }
        ui.label(RichText::new("Uma página por vez, agente nativo Edge/WebView2 e abertura em primeiro plano. Captcha é manual. Arquivos existentes nunca são substituídos.").small().color(MUTED));
    }

    fn content(&mut self, ui: &mut egui::Ui) -> Vec<(usize, egui::Rect)> {
        let completed = self
            .rows
            .iter()
            .filter(|r| matches!(r.status, Status::Completed | Status::NotIdentified))
            .count();
        let active = self.rows.iter().filter(|r| r.status.active()).count();
        let speed: f64 = self
            .rows
            .iter()
            .filter(|r| r.status.active())
            .map(|r| r.speed)
            .sum();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Downloads").size(22.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{}/s", bytes(speed.max(0.0) as u64)))
                        .monospace()
                        .size(16.0)
                        .color(ACCENT),
                )
                .on_hover_text("Velocidade total dos downloads ativos");
            });
        });
        ui.label(
            RichText::new(format!(
                "{active} / {} ativos · {completed} / {} concluídos",
                self.parallel,
                self.rows.len()
            ))
            .small()
            .color(MUTED),
        );
        if self.rows.len() > 5 || !self.search.is_empty() {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Filtrar downloads por nome ou caminho...")
                    .desired_width(f32::INFINITY),
            );
        }
        ui.separator();
        let query = self.search.to_lowercase();
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .filter(|row| row.path.to_string_lossy().to_lowercase().contains(&query))
            .collect();
        rows.sort_by_key(|row| (transfer_priority(&row.status), row.id));
        egui::ScrollArea::vertical().id_salt("downloads").auto_shrink([false, false]).show(ui, |ui| {
            if self.rows.is_empty() {
                ui.label(RichText::new("Nenhum download iniciado.").size(17.0).color(MUTED));
                ui.label(RichText::new("Selecione links, schema e pasta-base à esquerda. O progresso de cada arquivo e de suas partes aparecerá aqui.").small().color(MUTED));
            } else if rows.is_empty() {
                ui.label("Nenhum download corresponde ao filtro.");
            }
            rows.into_iter().map(|row| (row.id, transfer_row(ui, row))).collect()
        }).inner
    }

    fn activity(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("ATIVIDADE").size(11.0).color(ACCENT).strong());
        ui.label(RichText::new("Log de downloads").size(21.0).strong());
        ui.label(
            RichText::new("Eventos em tempo real · tempo da sessão")
                .small()
                .color(MUTED),
        );
        ui.horizontal(|ui| {
            if ui.small_button("Exportar").clicked() {
                self.export_log();
            }
            if ui.small_button("Limpar").clicked() {
                self.logs.clear();
            }
        });
        if let Some((notice, error)) = &self.notice {
            egui::Frame::new()
                .fill(CARD)
                .corner_radius(8)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(notice).small().color(if *error {
                            RED
                        } else {
                            ACCENT
                        }))
                        .wrap(),
                    );
                });
        }
        ui.separator();
        egui::Frame::new()
            .fill(Color32::from_rgb(9, 13, 20))
            .corner_radius(8)
            .inner_margin(10)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("logs")
                    .stick_to_bottom(true)
                    .max_height(ui.available_height().max(0.0))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.logs {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(line).monospace().size(11.0).color(MUTED),
                                )
                                .wrap(),
                            );
                        }
                    });
            });
    }

    fn workspace(&mut self, ctx: &egui::Context) -> WorkspaceLayout {
        let setup = egui::SidePanel::left("setup")
            .exact_width(SIDE_WIDTH)
            .resizable(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(20))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar")
                    .show(ui, |ui| self.sidebar(ui));
            })
            .response
            .rect;
        let activity = egui::SidePanel::right("activity")
            .exact_width(SIDE_WIDTH)
            .resizable(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(20))
            .show(ctx, |ui| self.activity(ui))
            .response
            .rect;
        let center = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(16))
            .show(ctx, |ui| self.content(ui));
        WorkspaceLayout {
            setup,
            activity,
            center: center.response.rect,
            cards: center.inner,
        }
    }

    fn browser_window(&mut self, ctx: &egui::Context) {
        let mut open = self.browser_window_open;
        egui::Window::new("Navegador / autenticação")
            .open(&mut open)
            .default_width(700.0)
            .show(ctx, |ui| self.browser_panel(ui, 480.0));
        self.browser_window_open = open;
    }

    fn browser_panel(&mut self, ui: &mut egui::Ui, height: f32) {
        let mut open_request = None;
        let mut skip_request = None;
        egui::ScrollArea::vertical().id_salt("browser_panel").max_height(height).auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("Autenticação assistida, sem copiar links manualmente").strong());
            ui.label(RichText::new("Somente arquivos que já ocupam uma vaga podem abrir captcha. Conclua o acesso e clique em Download no site: o mesmo arquivo continuará automaticamente. Pular arquivo libera uma vaga. Captcha é manual.").small().color(MUTED));
            ui.horizontal(|ui| {
                if ui.add_enabled(self.auth_scope_active() && self.browser_ready && !self.browser_opened.is_empty(), egui::Button::new("Mostrar janela do navegador")).clicked() {
                    if let Some(browser) = &self.browser { let _ = browser.send(BrowserCommand::Show); }
                }
                if ui.add_enabled(self.browser_ready, egui::Button::new("Minimizar")).clicked() {
                    if let Some(browser) = &self.browser { let _ = browser.send(BrowserCommand::Minimize); }
                }
            });
            ui.checkbox(&mut self.browser_only_pending, "Somente arquivos aguardando acesso");
            ui.label(RichText::new(format!("Agente nativo Edge/WebView2, sem Safari forçado. Uma página aberta; {} aguardando na fila. Conclua ou feche a página atual para liberar a próxima. Cloudflare ainda pode recusar WebViews.", self.browser_queue.len())).small().color(AMBER));
            if self.structure.is_none() { ui.label("Carregue o schema/estrutura para revisar os acessos."); return; }
            for (id, link) in self.links.iter().enumerate() {
                let Some(assignment) = self.assignments.get(id) else { continue; };
                let eligible = self.can_authorize(id);
                if self.browser_only_pending && !eligible { continue; }
                egui::Frame::new().fill(CARD).corner_radius(8).inner_margin(10).show(ui, |ui| {
                    ui.label(RichText::new(format!("Linha {} · {} · {}", link.line, link.provider.name(), link.hint.as_deref().unwrap_or("sem nome"))).strong());
                    ui.label(RichText::new(if self.browser_opened.contains(&id) { "Página atual · vaga reservada" } else if eligible && self.browser_queue.contains(&id) { "Aguarda a página atual · vaga reservada" } else if eligible { "Aguardando acesso · abra o navegador para continuar" } else if assignment.browser_access.is_some() { "Sessão capturada; nenhuma autenticação solicitada agora" } else { "Sem vaga ativa solicitando acesso. Inicie os downloads." }).small().color(MUTED));
                    if ui.add_enabled(eligible, egui::Button::new("Abrir captcha deste arquivo")).clicked() { open_request = Some(id); }
                    if ui.add_enabled(eligible, egui::Button::new("Pular arquivo")).clicked() { skip_request = Some(id); }
                });
            }
        });
        if let Some(id) = open_request {
            self.open_browser(id, true);
        }
        if let Some(id) = skip_request {
            self.skip_authorization(id);
        }
    }

    fn export_log(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("mldo-log.txt")
            .add_filter("Texto", &["txt"])
            .save_file()
        {
            let text = self.logs.iter().cloned().collect::<Vec<_>>().join("\r\n");
            match std::fs::write(path, text) {
                Ok(()) => self.log("Log exportado."),
                Err(error) => self.error(error),
            }
        }
    }

    fn review(&mut self, ctx: &egui::Context) {
        let mut open = self.review_open;
        egui::Window::new("Acessos / URLs diretas opcionais").open(&mut open).default_width(850.0).resizable(true).show(ctx, |ui| {
            ui.label("Este painel trata apenas de autenticação. Não há seleção de pasta por arquivo.");
            ui.label(RichText::new("O destino é identificado automaticamente após o download completo. Sem identificação segura, o arquivo vai para NotIdentify.").small().color(ACCENT));
            ui.checkbox(&mut self.only_pending, "Mostrar somente acessos sem sessão capturada");
            egui::ScrollArea::vertical().id_salt("review").max_height(510.0).show(ui, |ui| {
                for (index, (link, assignment)) in self.links.iter().zip(self.assignments.iter_mut()).enumerate() {
                    if self.only_pending && assignment.browser_access.is_some() { continue; }
                    egui::Frame::new().fill(CARD).corner_radius(8).inner_margin(12).show(ui, |ui| {
                        ui.label(RichText::new(format!("Linha {} · {} · {}", link.line, link.provider.name(), link.hint.as_deref().or_else(|| assignment.remote.as_ref().and_then(|remote| remote.name.as_deref())).unwrap_or("nome será conhecido após baixar"))).strong());
                        egui::CollapsingHeader::new("Acesso pelo navegador / link direto opcional").id_salt(("access", index)).show(ui, |ui| {
                            ui.add_enabled_ui(false, |ui| { ui.label("O navegador só abre para um arquivo ativo que esteja aguardando acesso, após Iniciar downloads."); });
                            ui.label(RichText::new("Se houver captcha/login, conclua no site e copie o endereço do botão Download (não a página). A URL fica apenas nesta sessão e não aparece no log.").small().color(MUTED));
                            ui.add(egui::TextEdit::singleline(&mut assignment.direct_url).hint_text("Cole aqui a URL direta autorizada, se necessário").desired_width(f32::INFINITY));
                        });
                    });
                    ui.add_space(3.0);
                }
            });
            ui.label(RichText::new("Para renovar uma sessão, desmarque o filtro acima. URLs e cookies ficam fora do log.").small().color(MUTED));
        });
        self.review_open = open;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.receive();
        self.receive_auxiliary();
        if ctx.input(|i| i.viewport().close_requested()) && self.running {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.cancel_downloads();
        }
        if self.closing && !self.running {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        egui::TopBottomPanel::top("brand")
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(24, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    logo(ui);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("MLDO").size(22.0).strong());
                        ui.label(
                            RichText::new("Multi Link Download Organizer")
                                .size(11.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(concat!("v", env!("CARGO_PKG_VERSION"), "  /  DESKTOP"))
                                .size(10.0)
                                .color(MUTED),
                        );
                        ui.label(
                            RichText::new(if self.running { "FILA ATIVA" } else { "PRONTO" })
                                .size(11.0)
                                .color(ACCENT),
                        );
                    });
                });
            });
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(24, 7)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("ORGANIZAÇÃO LOCAL · SEM TELEMETRIA")
                            .size(9.0)
                            .color(MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(
                                "Baixe apenas arquivos que você tem autorização para acessar.",
                            )
                            .size(10.0)
                            .color(MUTED),
                        );
                    });
                });
            });
        let layout = self.workspace(ctx);
        debug_assert!((layout.setup.width() - layout.activity.width()).abs() < 1.0);
        debug_assert!(
            layout.center.width() >= 0.0
                && layout.cards.iter().all(|(_, rect)| rect.width() >= 0.0)
        );
        if self.review_open && !self.running {
            self.review(ctx);
        }
        if self.browser_window_open {
            self.browser_window(ctx);
        }
        if self.running || self.browser.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancellation {
            cancel.cancel();
        }
    }
}

fn step(ui: &mut egui::Ui, number: &str, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(number).monospace().size(12.0).color(ACCENT));
        ui.label(RichText::new(title).strong());
    });
    ui.label(RichText::new(subtitle).small().color(MUTED));
}

fn file_button(ui: &mut egui::Ui, path: Option<&PathBuf>, placeholder: &str) -> egui::Response {
    let label = path
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| placeholder.into());
    let response = ui.add_sized(
        [ui.available_width(), 37.0],
        egui::Button::new(RichText::new(label).size(12.0)).truncate(),
    );
    if let Some(path) = path {
        response.on_hover_text(path.display().to_string())
    } else {
        response
    }
}

fn transfer_priority(status: &Status) -> u8 {
    if status.active() {
        0
    } else {
        match status {
            Status::Queued => 1,
            Status::Completed | Status::NotIdentified => 3,
            _ => 2,
        }
    }
}

fn part_rects(rect: egui::Rect, count: usize) -> Vec<egui::Rect> {
    if count == 0 {
        return Vec::new();
    }
    let gap = 3.0_f32.min(rect.width() / (2 * count) as f32);
    let width = ((rect.width() - gap * (count - 1) as f32) / count as f32).max(0.0);
    (0..count)
        .map(|index| {
            egui::Rect::from_min_size(
                rect.min + Vec2::new(index as f32 * (width + gap), 0.0),
                Vec2::new(width, rect.height()),
            )
        })
        .collect()
}

fn segmented_bar(ui: &mut egui::Ui, row: &Row) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), egui::Sense::hover());
    let rects = part_rects(rect, row.parts.len().max(1));
    for (index, rect) in rects.into_iter().enumerate() {
        ui.painter()
            .rect_filled(rect, 3, Color32::from_rgb(9, 13, 20));
        let part = row.parts.get(index);
        let fraction = part.and_then(PartProgress::fraction).unwrap_or(0.0);
        let fill = match row.status {
            Status::Completed | Status::Merging => Color32::from_rgb(28, 107, 86),
            Status::NotIdentified => Color32::from_rgb(104, 78, 25),
            Status::Failed(_) => Color32::from_rgb(126, 53, 64),
            Status::Cancelled => Color32::from_rgb(65, 74, 88),
            _ => Color32::from_rgb(33, 91, 143),
        };
        if fraction > 0.0 {
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    rect.min,
                    Vec2::new(rect.width() * fraction, rect.height()),
                ),
                3,
                fill,
            );
        }
        let label = match part {
            Some(part) => match part.fraction() {
                Some(progress) => format!("P{} {:.0}%", index + 1, progress * 100.0),
                None => format!("P{} ?", index + 1),
            },
            None => "Aguardando transferência".into(),
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            FontId::proportional(if rect.width() < 55.0 { 9.0 } else { 11.0 }),
            TEXT,
        );
        if let Some(part) = part {
            let detail = part
                .total
                .map(|total| format!("{} / {}", bytes(part.downloaded), bytes(total)))
                .unwrap_or_else(|| {
                    format!(
                        "{} recebidos; tamanho total não informado",
                        bytes(part.downloaded)
                    )
                });
            ui.interact(
                rect,
                ui.id().with((row.id, "part", index)),
                egui::Sense::hover(),
            )
            .on_hover_text(format!(
                "Parte {} de {}: {detail}",
                index + 1,
                row.parts.len()
            ));
        }
    }
}

fn transfer_row(ui: &mut egui::Ui, row: &Row) -> egui::Rect {
    let color = match row.status {
        Status::Completed => ACCENT,
        Status::Failed(_) => RED,
        Status::Cancelled | Status::PendingOrganization | Status::NotIdentified => AMBER,
        _ => MUTED,
    };
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(9)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                let name = row.path.file_name().unwrap_or_default().to_string_lossy();
                let speed_width = 108.0_f32.min(ui.available_width() * 0.4);
                let name_width =
                    (ui.available_width() - speed_width - ui.spacing().item_spacing.x).max(0.0);
                ui.add_sized(
                    [name_width, 18.0],
                    egui::Label::new(RichText::new(name).strong()).truncate(),
                )
                .on_hover_text(row.path.display().to_string());
                ui.allocate_ui_with_layout(
                    Vec2::new(speed_width, 18.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label(
                            RichText::new(format!("{}/s", bytes(row.speed.max(0.0) as u64)))
                                .monospace()
                                .size(13.0)
                                .color(ACCENT),
                        );
                    },
                );
            });
            segmented_bar(ui, row);
            let progress = if matches!(
                row.status,
                Status::Completed | Status::NotIdentified | Status::PendingOrganization
            ) {
                1.0
            } else {
                row.total
                    .filter(|t| *t > 0)
                    .map(|t| (row.downloaded as f64 / t as f64).min(1.0) as f32)
                    .unwrap_or(0.0)
            };
            ui.horizontal(|ui| {
                let size = row
                    .total
                    .map(|t| {
                        format!(
                            "{}/{} · {:.0}%",
                            bytes(row.downloaded),
                            bytes(t),
                            progress * 100.0
                        )
                    })
                    .unwrap_or_else(|| format!("{} · total desconhecido", bytes(row.downloaded)));
                let status_width = 134.0_f32.min(ui.available_width() * 0.5);
                let size_width =
                    (ui.available_width() - status_width - ui.spacing().item_spacing.x).max(0.0);
                ui.add_sized(
                    [size_width, 12.0],
                    egui::Label::new(RichText::new(&size).size(10.0).color(MUTED)).truncate(),
                )
                .on_hover_text(size);
                ui.allocate_ui_with_layout(
                    Vec2::new(status_width, 12.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label(RichText::new(row.status.label()).size(10.0).color(color));
                    },
                );
            });
            if let Status::Failed(message) = &row.status {
                ui.label(RichText::new(message).small().color(RED));
            }
            if let Some(pending) = &row.pending {
                ui.label(
                    RichText::new(format!(
                        "Completo em {} · corrija a falha de movimentação e reprocesse sem rede.",
                        pending.relative.display()
                    ))
                    .small()
                    .color(AMBER),
                );
            }
        })
        .response
        .rect
}

fn bytes(value: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut number = value as f64;
    let mut index = 0;
    while number >= 1024.0 && index < units.len() - 1 {
        number /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{value} B")
    } else {
        format!("{number:.1} {}", units[index])
    }
}

/// Fixtures visuais sem rede, excluídas do executável release distribuído.
#[cfg(any(test, debug_assertions))]
fn preview_rows(files: usize, parts: usize) -> Vec<Row> {
    (0..files)
        .map(|id| {
            let mut row = Row::queued(
                id,
                PathBuf::from(format!("Exemplo/dados/arquivo-{}.bin", id + 1)),
            );
            row.status = Status::Downloading;
            row.speed = (id + 1) as f64 * 1_350_000.0;
            row.parts = (0..parts)
                .map(|index| {
                    let total = 128 * 1024 * 1024;
                    PartProgress {
                        downloaded: total * ((index * 19 + id * 13 + 17) % 95 + 1) as u64 / 100,
                        total: Some(total),
                    }
                })
                .collect();
            row.downloaded = row.parts.iter().map(|part| part.downloaded).sum();
            row.total = row.parts.iter().map(|part| part.total).sum();
            row
        })
        .collect()
}

#[cfg(debug_assertions)]
fn debug_preview(mut app: App) -> App {
    let setting = std::env::var("MLDO_UI_PREVIEW").unwrap_or_default();
    let config = setting
        .split_once(':')
        .and_then(|(files, parts)| {
            Some((files.parse::<usize>().ok()?, parts.parse::<usize>().ok()?))
        })
        .filter(|(files, parts)| (1..=5).contains(files) && (1..=5).contains(parts));
    if let Some((files, parts)) = config {
        app.parallel = files;
        app.connections = parts;
        app.multiple_connections = parts > 1;
        app.rows = preview_rows(files, parts);
        app.notice = Some((
            "Prévia visual local: dados simulados, sem rede.".into(),
            false,
        ));
        for id in 0..files {
            app.log(format!("Prévia do arquivo {}: {parts} partes com progresso independente. Nenhum download real foi iniciado.", id + 1));
        }
    }
    app
}

fn logo(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(42.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 10, ACCENT);
    let center = rect.center();
    let stroke = Stroke::new(2.6_f32, BG);
    painter.line_segment(
        [center + Vec2::new(0.0, -11.0), center + Vec2::new(0.0, 3.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(-6.0, -2.0), center + Vec2::new(0.0, 4.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(6.0, -2.0), center + Vec2::new(0.0, 4.0)],
        stroke,
    );
    painter.line_segment(
        [
            center + Vec2::new(-10.0, 10.0),
            center + Vec2::new(10.0, 10.0),
        ],
        stroke,
    );
}

pub fn app_icon() -> egui::IconData {
    let mut rgba = Vec::with_capacity(64 * 64 * 4);
    for y in 0i32..64 {
        for x in 0i32..64 {
            let arrow = ((29..=34).contains(&x) && (14..=37).contains(&y))
                || ((33..=43).contains(&y) && ((x - 32).abs() - (43 - y)).abs() <= 2)
                || ((17..=46).contains(&x) && (49..=53).contains(&y));
            let color = if arrow { BG } else { ACCENT };
            rgba.extend_from_slice(&color.to_array());
        }
    }
    egui::IconData {
        rgba,
        width: 64,
        height: 64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captcha_pages_follow_active_requests_one_at_a_time() {
        for slots in 1..=5 {
            let mut app = App::with_context(&egui::Context::default());
            app.parallel = slots;
            app.rows = (0..10)
                .map(|id| Row::queued(id, format!("{id}.bin").into()))
                .collect();
            let (tx, rx) = mpsc::channel();
            app.receiver = Some(rx);
            assert_eq!(app.next_authorization(), None);
            app.running = true;
            let mut replies = Vec::new();
            for id in 0..slots {
                let (request, reply) = AuthorizationRequest::channel();
                replies.push(reply);
                tx.send(Event::State {
                    id,
                    status: Status::AwaitingAuthorization,
                })
                .unwrap();
                tx.send(Event::AuthorizationRequired {
                    id,
                    request,
                    automatic: true,
                })
                .unwrap();
            }
            app.receive();
            for (id, reply) in replies.iter_mut().enumerate() {
                assert_eq!(app.next_authorization(), Some(id));
                app.browser_opened.insert(id);
                assert_eq!(app.next_authorization(), None);
                assert!(!app.can_authorize(slots));
                let access = CapturedAccess {
                    url: format!("https://cdn.example/{id}"),
                    referer: "https://vikingfile.com/".into(),
                    cookie: String::new(),
                    filename: None,
                    user_agent: None,
                };
                assert!(app.grant_authorization(id, access));
                assert!(matches!(
                    reply.try_recv(),
                    Ok(mldo::authorization::AuthorizationResponse::Granted(_))
                ));
            }
            assert_eq!(app.next_authorization(), None);
        }
    }

    fn render_workspace(
        app: &mut App,
        width: f32,
        height: f32,
    ) -> (WorkspaceLayout, egui::FullOutput) {
        let ctx = egui::Context::default();
        let _styled = App::with_context(&ctx);
        let mut layout = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(width, height),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::TopBottomPanel::top("test_brand")
                    .exact_height(76.0)
                    .show(ctx, |_| {});
                egui::TopBottomPanel::bottom("test_footer")
                    .exact_height(28.0)
                    .show(ctx, |_| {});
                layout = Some(app.workspace(ctx));
            },
        );
        (layout.unwrap(), output)
    }

    #[test]
    fn one_to_five_downloads_and_parts_fit_between_equal_side_columns() {
        for (width, height) in [(1020.0, 720.0), (1180.0, 820.0), (1600.0, 900.0)] {
            for files in 1..=5 {
                for parts in 1..=5 {
                    let ctx = egui::Context::default();
                    let mut app = App::with_context(&ctx);
                    app.rows = preview_rows(files, parts);
                    let (layout, _) = render_workspace(&mut app, width, height);
                    assert!((layout.setup.width() - SIDE_WIDTH).abs() < 1.0);
                    assert!((layout.activity.width() - SIDE_WIDTH).abs() < 1.0);
                    assert!(layout.setup.right() <= layout.center.left() + 1.0);
                    assert!(layout.center.right() <= layout.activity.left() + 1.0);
                    assert_eq!(layout.cards.len(), files);
                    for (_, card) in &layout.cards {
                        assert!(layout.center.expand(1.0).contains_rect(*card), "{files} arquivos / {parts} partes, {width}x{height}: {card:?} fora de {:?}", layout.center);
                    }
                    assert!(layout
                        .cards
                        .windows(2)
                        .all(|pair| pair[0].1.bottom() <= pair[1].1.top()));
                }
            }
        }
    }

    #[test]
    fn active_transfers_stay_above_a_large_queued_or_completed_list() {
        let ctx = egui::Context::default();
        let mut app = App::with_context(&ctx);
        app.rows = (0..100)
            .map(|id| {
                let mut row = Row::queued(id, PathBuf::from(format!("arquivo-{id}.bin")));
                if id < 50 {
                    row.status = Status::Completed;
                }
                row
            })
            .collect();
        app.rows
            .extend(preview_rows(5, 5).into_iter().map(|mut row| {
                row.id += 100;
                row
            }));
        let (layout, _) = render_workspace(&mut app, 1020.0, 720.0);
        assert_eq!(
            layout
                .cards
                .iter()
                .take(5)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![100, 101, 102, 103, 104]
        );
        assert!(layout
            .cards
            .iter()
            .take(5)
            .all(|(_, rect)| layout.center.expand(1.0).contains_rect(*rect)));
    }

    #[test]
    fn segments_do_not_overlap_and_show_independent_fractions() {
        for width in [50.0, 316.0, 496.0] {
            for count in 1..=5 {
                let rect =
                    egui::Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(width, 22.0));
                let parts = part_rects(rect, count);
                assert_eq!(parts.len(), count);
                assert!(parts
                    .iter()
                    .all(|part| rect.expand(0.01).contains_rect(*part)));
                assert!(parts
                    .windows(2)
                    .all(|pair| pair[0].right() < pair[1].left()));
                assert!((parts.last().unwrap().right() - rect.right()).abs() < 0.01);
            }
        }
        assert_eq!(
            PartProgress {
                downloaded: 25,
                total: Some(100)
            }
            .fraction(),
            Some(0.25)
        );
        assert_eq!(
            PartProgress {
                downloaded: 80,
                total: Some(100)
            }
            .fraction(),
            Some(0.8)
        );
        assert_eq!(
            PartProgress {
                downloaded: 80,
                total: None
            }
            .fraction(),
            None
        );
    }

    #[test]
    fn part_events_handle_unknown_size_fallback_retry_and_merge_without_fake_segments() {
        let ctx = egui::Context::default();
        let mut app = App::with_context(&ctx);
        app.rows = vec![Row::queued(4, PathBuf::from("arquivo.bin"))];
        let (tx, rx) = mpsc::channel();
        app.receiver = Some(rx);
        tx.send(Event::State {
            id: 4,
            status: Status::Downloading,
        })
        .unwrap();
        let parts = vec![
            PartProgress {
                downloaded: 25,
                total: Some(100),
            },
            PartProgress {
                downloaded: 80,
                total: Some(100),
            },
        ];
        tx.send(Event::PartsProgress {
            id: 4,
            parts: parts.clone(),
        })
        .unwrap();
        tx.send(Event::Progress {
            id: 4,
            downloaded: 105,
            total: Some(200),
            speed: 500.0,
        })
        .unwrap();
        app.receive();
        assert_eq!(app.rows[0].parts, parts);
        assert_eq!(app.rows[0].downloaded, 105);
        tx.send(Event::PartsProgress {
            id: 4,
            parts: vec![PartProgress {
                downloaded: 0,
                total: None,
            }],
        })
        .unwrap();
        app.receive();
        assert_eq!(app.rows[0].parts.len(), 1);
        assert_eq!(app.rows[0].parts[0].fraction(), None);
        assert_eq!(app.rows[0].downloaded, 0);
        assert_eq!(app.rows[0].total, None);
        assert_eq!(app.rows[0].speed, 0.0);
        tx.send(Event::State {
            id: 4,
            status: Status::Resolving,
        })
        .unwrap();
        app.receive();
        assert!(app.rows[0].parts.is_empty());
        assert_eq!(app.rows[0].downloaded, 0);
        tx.send(Event::PartsProgress {
            id: 4,
            parts: vec![
                PartProgress {
                    downloaded: 100,
                    total: Some(100)
                };
                5
            ],
        })
        .unwrap();
        tx.send(Event::State {
            id: 4,
            status: Status::Merging,
        })
        .unwrap();
        app.receive();
        assert_eq!(app.rows[0].parts.len(), 5);
        assert!(app.rows[0]
            .parts
            .iter()
            .all(|part| part.fraction() == Some(1.0)));
        assert_eq!(app.rows[0].speed, 0.0);
    }

    #[test]
    fn log_text_is_wrapped_inside_the_right_column_not_in_the_center() {
        fn texts(shape: &egui::Shape, output: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => output.push((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        texts(shape, output);
                    }
                }
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        let mut app = App::with_context(&ctx);
        app.rows = preview_rows(5, 5);
        app.log("MARCADOR_LOG exclusivo: um caminho muito longo com detalhes da organização e da transferência deve quebrar linhas sem invadir o centro da interface.");
        let (layout, output) = render_workspace(&mut app, 1180.0, 820.0);
        let mut labels = Vec::new();
        for shape in output.shapes {
            texts(&shape.shape, &mut labels);
        }
        assert!(!labels
            .iter()
            .any(|(text, _)| text.contains("Menos links. Mais ordem.")));
        let (_, log) = labels
            .iter()
            .find(|(text, _)| text.contains("MARCADOR_LOG"))
            .unwrap();
        assert!(layout.activity.expand(1.0).contains_rect(*log));
        assert!(!layout.center.contains_rect(*log));
    }

    #[test]
    fn connection_multiplier_is_opt_in_and_separate_from_file_parallelism() {
        let ctx = egui::Context::default();
        let mut app = App::with_context(&ctx);
        assert!(!app.multiple_connections);
        assert_eq!(app.connections_per_file(), 1);
        app.multiple_connections = true;
        for count in 1..=5 {
            app.connections = count;
            assert_eq!(app.connections_per_file(), count);
            assert_eq!(app.parallel, 3);
        }
        app.multiple_connections = false;
        assert_eq!(app.connections_per_file(), 1);
    }
    #[test]
    fn both_json_datasets_enable_start_without_manual_assignments_even_with_count_difference() {
        let ctx = egui::Context::default();
        let mut app = App::with_context(&ctx);
        let root = tempfile::tempdir().unwrap();
        for (schema, links) in [
            (
                include_str!("../Prompt/download_schema.json"),
                include_str!("../Prompt/links.txt"),
            ),
            (
                include_str!("../tests/fixtures/upload_download_schema.json"),
                include_str!("../tests/fixtures/upload_links.txt"),
            ),
        ] {
            app.structure = Some(input::parse_structure(schema).unwrap());
            app.links = input::parse_links(links).unwrap();
            app.reset_mapping();
            app.destination = None;
            assert!(!app.can_start());
            app.destination = Some(root.path().to_path_buf());
            assert!(app.can_start());
            for assignment in &mut app.assignments {
                assignment.target = None;
            }
            assert!(app.can_start());
            assert_eq!(app.jobs().unwrap().len(), app.links.len());
            app.links.truncate(1);
            app.reset_mapping();
            assert!(app.can_start());
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.sidebar(ui));
            });
        }
    }

    #[test]
    fn formats_human_readable_sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(app_icon().rgba.len(), 64 * 64 * 4);
    }
}
