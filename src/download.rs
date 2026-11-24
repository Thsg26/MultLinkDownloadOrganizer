use std::{
    path::{Path, PathBuf},
    sync::{mpsc::Sender, Arc},
    thread,
    time::{Duration, Instant},
};

use futures_util::{stream, StreamExt};
use reqwest::header;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::{
    authorization::{AuthorizationRequest, AuthorizationResponse},
    error::{Error, Result},
    hosts::{Resolved, Resolver},
    input::Structure,
    mapping::Job,
    safety,
    staging::{self, Coordinator, Outcome, PendingDownload},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Queued,
    Resolving,
    AwaitingAuthorization,
    Downloading,
    Merging,
    Retrying(u8),
    Completed,
    NotIdentified,
    Failed(String),
    Cancelled,
    PendingOrganization,
}

impl Status {
    pub fn label(&self) -> &str {
        match self {
            Self::Queued => "Na fila",
            Self::Resolving => "Obtendo link",
            Self::AwaitingAuthorization => "Aguardando captcha / acesso",
            Self::Downloading => "Baixando",
            Self::Merging => "Mesclando e verificando",
            Self::Retrying(_) => "Tentando novamente",
            Self::Completed => "Concluído",
            Self::NotIdentified => "Concluído · NotIdentify",
            Self::Failed(_) => "Falhou",
            Self::Cancelled => "Cancelado",
            Self::PendingOrganization => "Baixado · falha ao mover",
        }
    }
    pub fn active(&self) -> bool {
        matches!(
            self,
            Self::Resolving
                | Self::AwaitingAuthorization
                | Self::Downloading
                | Self::Merging
                | Self::Retrying(_)
        )
    }
}

/// Snapshot dos bytes de uma parte real da transferência, não uma fração
/// inventada do progresso agregado. Total ausente significa tamanho desconhecido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

impl PartProgress {
    pub fn fraction(&self) -> Option<f32> {
        self.total.map(|total| {
            if total == 0 {
                1.0
            } else {
                (self.downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32
            }
        })
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    AuthorizationRequired {
        id: usize,
        request: AuthorizationRequest,
        automatic: bool,
    },
    State {
        id: usize,
        status: Status,
    },
    Progress {
        id: usize,
        downloaded: u64,
        total: Option<u64>,
        speed: f64,
    },
    PartsProgress {
        id: usize,
        parts: Vec<PartProgress>,
    },
    Identified {
        id: usize,
        filename: String,
    },
    Organized {
        id: usize,
        relative: PathBuf,
    },
    Unidentified {
        id: usize,
        relative: PathBuf,
        reason: String,
    },
    Pending {
        id: usize,
        download: PendingDownload,
    },
    Log(String),
    Fatal(String),
    Finished,
}

pub struct Request {
    pub destination: PathBuf,
    pub structure: Structure,
    pub jobs: Vec<Job>,
    pub parallel: usize,
    /// 1 mantém o streaming normal; 2–5 tentam intervalos HTTP validados.
    pub connections: usize,
    /// Desktop mantém a vaga enquanto aguarda interação; usos sem UI podem falhar normalmente.
    pub interactive_auth: bool,
}

/// Publicar a mesma amostra para os segmentos e para o total do arquivo.
pub(crate) fn report_progress(id: usize, parts: Vec<PartProgress>, speed: f64, tx: &Sender<Event>) {
    let downloaded = parts.iter().map(|part| part.downloaded).sum();
    let total = parts.iter().map(|part| part.total).sum();
    let _ = tx.send(Event::PartsProgress { id, parts });
    let _ = tx.send(Event::Progress {
        id,
        downloaded,
        total,
        speed,
    });
}

pub fn start(request: Request, tx: Sender<Event>) -> Result<CancellationToken> {
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    thread::Builder::new().name("mldo-downloads".into()).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build()?;
            runtime.block_on(run(request, tx.clone(), worker_cancel))
        }));
        match outcome {
            Ok(Ok(())) => {},
            Ok(Err(error)) => { let _ = tx.send(Event::Fatal(error.to_string())); },
            Err(_) => { let _ = tx.send(Event::Fatal("O mecanismo de download foi interrompido por um erro interno. Reinicie a fila.".into())); },
        }
        let _ = tx.send(Event::Finished);
    })?;
    Ok(cancel)
}

async fn run(request: Request, tx: Sender<Event>, cancel: CancellationToken) -> Result<()> {
    if !(1..=5).contains(&request.parallel) {
        return Err(Error::Input(
            "Selecione entre 1 e 5 downloads simultâneos.".into(),
        ));
    }
    if !(1..=5).contains(&request.connections) {
        return Err(Error::Input(
            "Selecione entre 1 e 5 conexões por arquivo.".into(),
        ));
    }
    if cancel.is_cancelled() {
        return Ok(());
    }
    let root = safety::prepare(&request.destination, &request.structure)?;
    let _ = tx.send(Event::Log(format!(
        "Base/temporário preparados. {} arquivos; até {} simultâneos, {} conexões por arquivo (máximo de {} transferências HTTP de partes).",
        request.jobs.len(),
        request.parallel,
        request.connections,
        request.parallel * request.connections
    )));
    let resolver = Resolver::new()?;
    let coordinator = Arc::new(Coordinator::new(request.structure.clone()));
    let mut recovered = std::collections::HashSet::new();
    for mut record in staging::load(&root, &request.structure)? {
        if cancel.is_cancelled() {
            break;
        }
        let matching: Vec<_> = request
            .jobs
            .iter()
            .filter(|job| staging::source_key(job.link.url.as_str()) == record.source_key)
            .collect();
        if matching.len() != 1 {
            continue;
        }
        record.id = matching[0].id;
        record.hint = matching[0].link.hint.clone();
        let outcome = match coordinator.organize(&root, &record) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = tx.send(Event::Log(format!(
                    "Linha {}: registro de recuperação ignorado: {error}",
                    record.id + 1
                )));
                continue;
            }
        };
        report_progress(
            record.id,
            vec![PartProgress {
                downloaded: record.bytes,
                total: Some(record.bytes),
            }],
            0.0,
            &tx,
        );
        let status = publish_outcome(record.id, outcome, &tx);
        recovered.insert(record.id);
        let _ = tx.send(Event::Log(format!(
            "Linha {}: completo recuperado sem baixar novamente.",
            record.id + 1
        )));
        let _ = tx.send(Event::State {
            id: record.id,
            status,
        });
    }
    stream::iter(
        request
            .jobs
            .into_iter()
            .filter(|job| !recovered.contains(&job.id)),
    )
    .for_each_concurrent(request.parallel, |job| {
        let root = root.clone();
        let resolver = Arc::clone(&resolver);
        let tx = tx.clone();
        let cancel = cancel.clone();
        let coordinator = coordinator.clone();
        async move {
            process_job(
                job,
                JobWorker {
                    root: &root,
                    resolver: &resolver,
                    tx: &tx,
                    cancel: &cancel,
                    coordinator: &coordinator,
                    connections: request.connections,
                    interactive_auth: request.interactive_auth,
                },
            )
            .await;
        }
    })
    .await;
    Ok(())
}

struct JobWorker<'a> {
    root: &'a Path,
    resolver: &'a Resolver,
    tx: &'a Sender<Event>,
    cancel: &'a CancellationToken,
    coordinator: &'a Coordinator,
    connections: usize,
    interactive_auth: bool,
}

async fn process_job(mut job: Job, worker: JobWorker<'_>) {
    let JobWorker {
        root,
        resolver,
        tx,
        cancel,
        coordinator,
        connections,
        interactive_auth,
    } = worker;
    let mut result = Err(Error::Cancelled);
    let mut attempt = 1u8;
    let mut authorizations = 0usize;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let _ = tx.send(Event::State {
            id: job.id,
            status: Status::Resolving,
        });
        result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = async {
                let mut resolved = resolver.resolve(&job).await?;
                // Nenhuma associação/tamanho presumido pode impedir o recebimento.
                // A validação de transporte permanece; o schema só é aplicado após EOF.
                resolved.filename = resolved.filename.or_else(|| job.link.hint.clone().and_then(safe_filename));
                let parent = staging::directory(root)?;
                let received = receive_download(job.id, &parent, resolver, resolved, tx, cancel, connections).await?;
                if cancel.is_cancelled() { return Err(Error::Cancelled); }
                let outcome = coordinator.commit(root, &job, received.temp, received.bytes, received.filename)?;
                Ok(publish_outcome(job.id, outcome, tx))
            } => result,
        };
        if interactive_auth
            && result
                .as_ref()
                .is_err_and(|error| error.needs_authorization())
        {
            if cancel.is_cancelled() {
                result = Err(Error::Cancelled);
                break;
            }
            let (request, response) = AuthorizationRequest::channel();
            let _ = tx.send(Event::State {
                id: job.id,
                status: Status::AwaitingAuthorization,
            });
            let _ = tx.send(Event::Log(format!("{}: aguardando liberação manual; a vaga continua reservada, sem tentar os próximos links.", job.label())));
            if tx
                .send(Event::AuthorizationRequired {
                    id: job.id,
                    request,
                    automatic: authorizations == 0,
                })
                .is_err()
            {
                result = Err(Error::Cancelled);
                break;
            }
            authorizations += 1;
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                response = response => response.ok(),
            };
            match response {
                Some(AuthorizationResponse::Granted(access)) => {
                    if let Err(error) = crate::browser::validate_capture(&access) {
                        result = Err(error);
                        break;
                    }
                    match url::Url::parse(&access.url) {
                        Ok(url) => job.direct_url = Some(url),
                        Err(_) => {
                            result = Err(Error::Host("URL autorizada inválida.".into()));
                            break;
                        }
                    }
                    job.browser_access = Some(access);
                    attempt = 1;
                    continue; // o mesmo job e a mesma vaga, sem reiniciar a fila
                }
                Some(AuthorizationResponse::Skip) | None => {
                    result = Err(Error::Cancelled);
                    break;
                }
            }
        }
        if result.as_ref().is_err_and(|error| error.retryable()) && attempt < 3 {
            let _ = tx.send(Event::State {
                id: job.id,
                status: Status::Retrying(attempt + 1),
            });
            let _ = tx.send(Event::Log(format!(
                "{}: falha temporária; tentativa {} de 3 em {} segundos.",
                job.label(),
                attempt + 1,
                attempt * 2
            )));
            tokio::select! {
                _ = cancel.cancelled() => { result = Err(Error::Cancelled); break; },
                _ = tokio::time::sleep(Duration::from_secs(u64::from(attempt) * 2)) => {},
            }
            attempt += 1;
        } else {
            break;
        }
    }
    let status = match result {
        Ok(status) => status,
        Err(Error::Cancelled) => Status::Cancelled,
        Err(error) => Status::Failed(error.to_string()),
    };
    let detail = match &status {
        Status::Failed(message) => message.as_str(),
        _ => status.label(),
    };
    let _ = tx.send(Event::Log(format!("{} — {detail}", job.label())));
    let _ = tx.send(Event::State { id: job.id, status });
}

fn publish_outcome(id: usize, outcome: Outcome, tx: &Sender<Event>) -> Status {
    match outcome {
        Outcome::Organized(relative) => {
            let _ = tx.send(Event::Log(format!(
                "Linha {}: movido para {}.",
                id + 1,
                relative.display()
            )));
            let _ = tx.send(Event::Organized { id, relative });
            Status::Completed
        }
        Outcome::Unidentified { relative, reason } => {
            let _ = tx.send(Event::Log(format!(
                "Linha {}: movido para {}. {reason}",
                id + 1,
                relative.display()
            )));
            let _ = tx.send(Event::Unidentified {
                id,
                relative,
                reason,
            });
            Status::NotIdentified
        }
        Outcome::Pending(download) => {
            let _ = tx.send(Event::Log(download.reason.clone()));
            let _ = tx.send(Event::Pending { id, download });
            Status::PendingOrganization
        }
    }
}

#[cfg(test)]
async fn transfer(
    id: usize,
    root: &Path,
    relative: &Path,
    resolver: &Resolver,
    resolved: Resolved,
    tx: &Sender<Event>,
    cancel: &CancellationToken,
) -> Result<()> {
    let target = safety::checked_path(root, relative)?;
    if target.try_exists()? {
        return Err(Error::Input(format!(
            "O arquivo {} já existe e não será sobrescrito.",
            relative.display()
        )));
    }
    let parent = target
        .parent()
        .ok_or_else(|| Error::Input("Pasta de destino inválida.".into()))?;
    let received = receive_file(id, parent, Some(relative), resolver, resolved, tx, cancel).await?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    safety::checked_path(root, relative)?;
    received
        .temp
        .persist_noclobber(&target)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

pub(crate) struct Received {
    pub temp: tempfile::NamedTempFile,
    pub bytes: u64,
    pub filename: Option<String>,
}

async fn receive_download(
    id: usize,
    parent: &Path,
    resolver: &Resolver,
    resolved: Resolved,
    tx: &Sender<Event>,
    cancel: &CancellationToken,
    connections: usize,
) -> Result<Received> {
    if connections > 1 {
        match crate::segmented::receive(id, parent, resolver, &resolved, tx, cancel, connections)
            .await?
        {
            crate::segmented::Attempt::Ready(received) => return Ok(received),
            crate::segmented::Attempt::Fallback(reason) => {
                let _ = tx.send(Event::Log(format!(
                    "Linha {}: {reason} Usando uma conexão, sem reutilizar partes incompatíveis.",
                    id + 1
                )));
                report_progress(
                    id,
                    vec![PartProgress {
                        downloaded: 0,
                        total: None,
                    }],
                    0.0,
                    tx,
                );
            }
        }
    }
    receive_file(id, parent, None, resolver, resolved, tx, cancel).await
}

pub(crate) fn file_request(resolver: &Resolver, resolved: &Resolved) -> reqwest::RequestBuilder {
    let mut request = resolver
        .client
        .get(resolved.url.clone())
        .header(header::REFERER, resolved.referer.as_str())
        .header(header::ACCEPT_ENCODING, "identity");
    if let Some(cookie) = &resolved.cookie {
        request = request.header(header::COOKIE, cookie);
    }
    if let Some(user_agent) = &resolved.user_agent {
        request = request.header(header::USER_AGENT, user_agent);
    }
    request
}

pub(crate) fn filename_and_type(
    response: &reqwest::Response,
    resolved: &Resolved,
    relative: Option<&Path>,
) -> Result<Option<String>> {
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let filename = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::hosts::disposition_filename)
        .or_else(|| resolved.filename.clone())
        .and_then(safe_filename);
    let json_file = relative
        .and_then(Path::extension)
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        || filename
            .as_ref()
            .is_some_and(|name| name.to_lowercase().ends_with(".json"));
    if content_type.contains("text/html")
        || (content_type.contains("application/json") && !json_file)
    {
        return Err(Error::Authorization("O site retornou uma página HTML/JSON em vez do arquivo. Libere o acesso no Navegador ou informe uma URL direta em Acessos.".into()));
    }
    Ok(filename)
}

pub(crate) fn reject_html(prefix: &[u8]) -> Result<()> {
    let start = String::from_utf8_lossy(prefix)
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_ascii_lowercase();
    if start.starts_with("<!doctype html") || start.starts_with("<html") {
        return Err(Error::Authorization("Recebida uma página HTML disfarçada de arquivo. Download descartado; obtenha uma URL direta válida.".into()));
    }
    Ok(())
}

async fn receive_file(
    id: usize,
    parent: &Path,
    relative: Option<&Path>,
    resolver: &Resolver,
    resolved: Resolved,
    tx: &Sender<Event>,
    cancel: &CancellationToken,
) -> Result<Received> {
    let response = file_request(resolver, &resolved)
        .send()
        .await?
        .error_for_status()?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(Error::Host(
            "O servidor não retornou um arquivo completo (HTTP 200).".into(),
        ));
    }
    let filename = filename_and_type(&response, &resolved, relative)?;
    let total = response.content_length();
    let temp = tempfile::Builder::new()
        .prefix(&format!(".mldo-linha-{}-", id + 1))
        .suffix(if relative.is_some() {
            ".part"
        } else {
            ".download"
        })
        .tempfile_in(parent)?;
    let mut file = tokio::fs::File::from_std(temp.reopen()?);
    let _ = tx.send(Event::State {
        id,
        status: Status::Downloading,
    });
    if relative.is_none() {
        if let Some(filename) = &filename {
            let _ = tx.send(Event::Identified {
                id,
                filename: filename.clone(),
            });
        }
    }
    report_progress(
        id,
        vec![PartProgress {
            downloaded: 0,
            total,
        }],
        0.0,
        tx,
    );
    let mut chunks = response.bytes_stream();
    let mut downloaded = 0u64;
    let mut sample_bytes = 0u64;
    let mut sample_time = Instant::now();
    let mut prefix = Vec::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(350));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = ticker.tick() => {
                let speed = (downloaded - sample_bytes) as f64 / sample_time.elapsed().as_secs_f64().max(0.001);
                report_progress(id, vec![PartProgress { downloaded, total }], speed, tx);
                sample_bytes = downloaded;
                sample_time = Instant::now();
            },
            chunk = chunks.next() => {
                let Some(chunk) = chunk else { break; };
                let chunk = chunk?;
                if prefix.len() < 512 {
                    prefix.extend_from_slice(&chunk[..chunk.len().min(512 - prefix.len())]);
                    reject_html(&prefix)?;
                }
                file.write_all(&chunk).await?;
                downloaded += chunk.len() as u64;
            }
        }
    }
    if total.is_some_and(|length| length != downloaded) {
        return Err(Error::Connection);
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    report_progress(
        id,
        vec![PartProgress {
            downloaded,
            total: Some(downloaded),
        }],
        0.0,
        tx,
    );
    Ok(Received {
        temp,
        bytes: downloaded,
        filename,
    })
}

fn safe_filename(name: String) -> Option<String> {
    let normalized = name.replace('\\', "/");
    let name = normalized.rsplit('/').next()?;
    if name.chars().count() > 255 {
        return None;
    }
    crate::input::relative_path(name)
        .ok()
        .filter(|path| path.components().count() == 1)
        .map(|_| name.to_owned())
}

/// Publica arquivos já baixados. Não realiza nenhuma requisição HTTP.
pub fn organize_downloads(
    destination: PathBuf,
    structure: Structure,
    entries: Vec<PendingDownload>,
    tx: Sender<Event>,
) -> Result<CancellationToken> {
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    thread::Builder::new()
        .name("mldo-organize".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let root = safety::prepare(&destination, &structure)?;
                let coordinator = Coordinator::new(structure);
                for pending in entries {
                    if worker_cancel.is_cancelled() {
                        let _ = tx.send(Event::Log(
                            "Organização cancelada; os downloads completos foram preservados."
                                .into(),
                        ));
                        break;
                    }
                    match coordinator.organize(&root, &pending) {
                        Ok(outcome) => {
                            let status = publish_outcome(pending.id, outcome, &tx);
                            let _ = tx.send(Event::State {
                                id: pending.id,
                                status,
                            });
                        }
                        Err(error) => {
                            let _ = tx.send(Event::Log(format!(
                                "Linha {}: {error} O download completo continua preservado.",
                                pending.id + 1
                            )));
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                let _ = tx.send(Event::Fatal(error.to_string()));
            }
            let _ = tx.send(Event::Finished);
        })?;
    Ok(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        input::{parse_links, parse_structure},
        mapping::{build_jobs, suggest},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use tokio::{
        io::AsyncReadExt,
        net::{TcpListener, TcpStream},
    };

    #[test]
    fn progress_snapshots_sum_real_parts_without_inventing_unknown_totals() {
        let (tx, rx) = mpsc::channel();
        let parts = vec![
            PartProgress {
                downloaded: 20,
                total: Some(100),
            },
            PartProgress {
                downloaded: 70,
                total: Some(100),
            },
        ];
        report_progress(8, parts.clone(), 1234.0, &tx);
        assert!(
            matches!(rx.recv().unwrap(), Event::PartsProgress { id: 8, parts: actual } if actual == parts)
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Event::Progress {
                id: 8,
                downloaded: 90,
                total: Some(200),
                speed: 1234.0
            }
        ));
        report_progress(
            8,
            vec![PartProgress {
                downloaded: 25,
                total: None,
            }],
            100.0,
            &tx,
        );
        assert!(
            matches!(rx.recv().unwrap(), Event::PartsProgress { parts, .. } if parts[0].fraction().is_none())
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Event::Progress {
                downloaded: 25,
                total: None,
                ..
            }
        ));
    }

    async fn read_request(socket: &mut TcpStream) {
        let mut header = Vec::new();
        let mut chunk = [0u8; 1024];
        while !header.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(
                count > 0,
                "Cliente desconectou antes de enviar os cabeçalhos"
            );
            header.extend_from_slice(&chunk[..count]);
            assert!(header.len() <= 8192);
        }
    }

    async fn server(
        html: bool,
        slow: bool,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let max_clone = max.clone();
        let handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let active = active.clone();
                let max = max_clone.clone();
                tokio::spawn(async move {
                    read_request(&mut socket).await;
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(count, Ordering::SeqCst);
                    let body = if html {
                        b"<!DOCTYPE html><html>captcha</html>".to_vec()
                    } else {
                        vec![42u8; 8192]
                    };
                    let pieces = if slow { 100 } else { 5 };
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len() * pieces);
                    if socket.write_all(header.as_bytes()).await.is_ok() {
                        for piece in 0..pieces {
                            if piece > 0 {
                                tokio::time::sleep(Duration::from_millis(15)).await;
                            }
                            if socket.write_all(&body).await.is_err() {
                                break;
                            }
                        }
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        (address, max, handle)
    }

    fn plan(root: &Path, address: &str, count: usize, parallel: usize) -> Request {
        let text = (0..count)
            .map(|i| format!("Game/file{i}.bin"))
            .collect::<Vec<_>>()
            .join("\n");
        let structure = parse_structure(&text).unwrap();
        let text = (0..count)
            .map(|i| format!("https://vikingfile.com/f/{i}#file{i}.bin"))
            .collect::<Vec<_>>()
            .join("\n");
        let links = parse_links(&text).unwrap();
        let mut assignments = suggest(&links, &structure);
        for assignment in &mut assignments {
            assignment.direct_url = format!("{address}/file");
        }
        let jobs = build_jobs(&links, &structure, &assignments).unwrap();
        Request {
            destination: root.to_owned(),
            structure,
            jobs,
            parallel,
            connections: 1,
            interactive_auth: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_for_captcha_reserves_each_slot_until_download_completes() {
        let root = tempfile::tempdir().unwrap();
        let (blocked, _, challenge_server) = server(true, false).await;
        let (allowed, _, file_server) = server(false, false).await;
        let mut request = plan(root.path(), &blocked, 4, 2);
        request.interactive_auth = true;
        let (tx, rx) = mpsc::channel();
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(run(request, tx, cancel.clone()));
        let mut held = Vec::new();
        let mut granted = None;
        let mut completed = None;
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("queue event");
            match event {
                Event::AuthorizationRequired {
                    id,
                    request,
                    automatic,
                } => {
                    assert!(automatic);
                    if id >= 2 {
                        assert!(granted.is_some());
                        assert_eq!(completed, granted);
                        break;
                    }
                    if !held.is_empty() {
                        request
                            .grant(crate::browser::CapturedAccess {
                                url: format!("{allowed}/file"),
                                referer: blocked.clone(),
                                cookie: String::new(),
                                filename: None,
                                user_agent: None,
                            })
                            .unwrap();
                        granted = Some(id);
                    } else {
                        held.push(request);
                    }
                }
                Event::State {
                    id,
                    status: Status::Completed,
                } => completed = Some(id),
                Event::State {
                    id,
                    status: Status::Resolving,
                } if id >= 2 => assert!(completed.is_some()),
                _ => {}
            }
        }
        cancel.cancel();
        worker.await.unwrap().unwrap();
        assert!(!held[0].is_pending());
        challenge_server.abort();
        file_server.abort();
    }

    #[tokio::test]
    async fn downloads_to_correct_paths_and_enforces_parallel_limit() {
        for limit in 1..=5 {
            let root = tempfile::tempdir().unwrap();
            let (address, max, server) = server(false, false).await;
            let (tx, rx) = mpsc::channel();
            run(
                plan(root.path(), &address, 6, limit),
                tx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(max.load(Ordering::SeqCst) <= limit);
            assert!(max.load(Ordering::SeqCst) > 0);
            assert_eq!(
                rx.try_iter()
                    .filter(|e| matches!(
                        e,
                        Event::State {
                            status: Status::Completed,
                            ..
                        }
                    ))
                    .count(),
                6
            );
            for i in 0..6 {
                assert_eq!(
                    std::fs::metadata(root.path().join(format!("Game/file{i}.bin")))
                        .unwrap()
                        .len(),
                    8192 * 5
                );
            }
            assert!(!std::fs::read_dir(root.path().join("Game"))
                .unwrap()
                .any(|p| p.unwrap().path().extension().is_some_and(|e| e == "part")));
            server.abort();
        }
    }

    #[tokio::test]
    async fn rejects_html_without_publishing_or_leaving_parts() {
        let root = tempfile::tempdir().unwrap();
        let (address, _, server) = server(true, false).await;
        let (tx, rx) = mpsc::channel();
        run(
            plan(root.path(), &address, 1, 1),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(rx.try_iter().any(|e| matches!(
            e,
            Event::State {
                status: Status::Failed(_),
                ..
            }
        )));
        assert_eq!(
            std::fs::read_dir(root.path().join("Game")).unwrap().count(),
            0
        );
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_cleans_up_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let (address, _, server) = server(false, true).await;
        let (tx, rx) = mpsc::channel();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let request = plan(root.path(), &address, 3, 2);
        let task = tokio::spawn(async move { run(request, tx, task_cancel).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        task.await.unwrap().unwrap();
        assert_eq!(
            rx.try_iter()
                .filter(|e| matches!(
                    e,
                    Event::State {
                        status: Status::Cancelled,
                        ..
                    }
                ))
                .count(),
            3
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("Game")).unwrap().count(),
            0
        );
        server.abort();
    }

    async fn transfer_response(raw: &'static [u8]) -> (Result<()>, tempfile::TempDir, Vec<Event>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            url::Url::parse(&format!("http://{}/file", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(raw).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let resolver = Resolver::new().unwrap();
        let (tx, rx) = mpsc::channel();
        let result = transfer(
            0,
            root.path(),
            Path::new("test.bin"),
            &resolver,
            Resolved {
                url: url.clone(),
                referer: url,
                cookie: None,
                user_agent: None,
                filename: None,
            },
            &tx,
            &CancellationToken::new(),
        )
        .await;
        server.await.unwrap();
        (result, root, rx.try_iter().collect())
    }

    #[tokio::test]
    async fn accepts_unknown_length_and_reports_final_size() {
        let (result, root, events) = transfer_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\nhello").await;
        result.unwrap();
        assert_eq!(
            std::fs::read(root.path().join("test.bin")).unwrap(),
            b"hello"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Progress { total: None, .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Progress {
                downloaded: 5,
                total: Some(5),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn rejects_truncated_body_and_html_headers() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops".as_slice(),
        ] {
            let (result, root, _) = transfer_response(response).await;
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn retries_temporary_http_failure_then_completes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for response in [
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".as_slice(),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket.write_all(response).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        tokio::time::timeout(
            Duration::from_secs(8),
            run(
                plan(root.path(), &address, 1, 1),
                tx,
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::State {
                status: Status::Retrying(2),
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::State {
                status: Status::Completed,
                ..
            }
        )));
        assert_eq!(
            std::fs::read(root.path().join("Game/file0.bin")).unwrap(),
            b"ok"
        );
    }

    #[tokio::test]
    async fn never_overwrites_file_created_during_transfer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            url::Url::parse(&format!("http://{}/file", listener.local_addr().unwrap())).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ndata")
                .await
                .unwrap();
            started_tx.send(()).unwrap();
            continue_rx.await.unwrap();
            socket.write_all(b"rest").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let resolver = Resolver::new().unwrap();
        let (tx, _) = mpsc::channel();
        let cancel = CancellationToken::new();
        let (result, ()) = tokio::join!(
            transfer(
                0,
                root.path(),
                Path::new("test.bin"),
                &resolver,
                Resolved {
                    url: url.clone(),
                    referer: url,
                    cookie: None,
                    user_agent: None,
                    filename: None
                },
                &tx,
                &cancel
            ),
            async {
                started_rx.await.unwrap();
                std::fs::write(root.path().join("test.bin"), b"original").unwrap();
                continue_tx.send(()).unwrap();
            }
        );
        server.await.unwrap();
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(root.path().join("test.bin")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    async fn unnamed_response(
        raw: &[u8],
        schema: &str,
    ) -> (tempfile::TempDir, Structure, Vec<Event>) {
        response_for_link(raw, schema, None).await
    }

    async fn response_for_link(
        raw: &[u8],
        schema: &str,
        hint: Option<&str>,
    ) -> (tempfile::TempDir, Structure, Vec<Event>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}/opaque", listener.local_addr().unwrap());
        let raw = raw.to_vec();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(&raw).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let structure = parse_structure(schema).unwrap();
        let links = parse_links(&format!(
            "https://vikingfile.com/f/opaque1{}",
            hint.map(|hint| format!("#{hint}")).unwrap_or_default()
        ))
        .unwrap();
        let mut assignments = suggest(&links, &structure);
        for assignment in &mut assignments {
            assignment.direct_url = address.clone();
        }
        let mut jobs = build_jobs(&links, &structure, &assignments).unwrap();
        jobs.truncate(1); // uma parte da fila, como ocorre ao tentar pendências
        let (tx, rx) = mpsc::channel();
        run(
            Request {
                destination: root.path().into(),
                structure: structure.clone(),
                jobs,
                parallel: 2,
                connections: 1,
                interactive_auth: false,
            },
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        (root, structure, rx.try_iter().collect())
    }

    #[tokio::test]
    async fn unnamed_link_is_organized_from_content_disposition_after_download() {
        let (root, _, events) = unnamed_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename*=UTF-8''real%20file.bin\r\nContent-Length: 8\r\nConnection: close\r\n\r\n12345678", "Game/other.bin\nGame/data/real file.bin").await;
        assert_eq!(
            std::fs::read(root.path().join("Game/data/real file.bin")).unwrap(),
            b"12345678"
        );
        assert!(events.iter().any(|event| matches!(event, Event::Identified { filename, .. } if filename == "real file.bin")));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::Completed,
                ..
            }
        )));
        assert!(!root.path().join("Game/other.bin").exists());
    }

    #[tokio::test]
    async fn supplied_json_datasets_organize_synthetic_local_bodies_with_exact_lengths() {
        // Corpos sintéticos locais; nunca acessa os links/anexos binários reais.
        let (root, _, events) = response_for_link(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
            include_str!("../Prompt/download_schema.json"),
            Some("Update.alignmentchunk"),
        )
        .await;
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("GTA6.Xbox.v1.0.2.1-CYBERLEEK/Update.alignmentchunk")
            )
            .unwrap(),
            b"x"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::Completed,
                ..
            }
        )));
        let mut raw =
            b"HTTP/1.1 200 OK\r\nContent-Length: 1595\r\nConnection: close\r\n\r\n".to_vec();
        raw.extend(vec![42; 1595]);
        let (root, _, events) = unnamed_response(
            &raw,
            include_str!("../tests/fixtures/upload_download_schema.json"),
        )
        .await;
        assert_eq!(
            std::fs::read(root.path().join("Uploadteste/DLSSVideoPlayer.ini")).unwrap(),
            vec![42; 1595]
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::Completed,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn named_size_mismatch_is_downloaded_once_then_sent_to_notidentify() {
        let schema = r#"[{"Tipo":"arquivo","Nome":"a.bin","Caminho":".\\Game\\a.bin","Tamanho_Bytes":5,"Criado_Em":"2026-11-21 10:00"}]"#;
        for raw in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n12345678".as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n12345678".as_slice(),
        ] {
            let (root, _, events) = response_for_link(raw, schema, Some("a.bin")).await;
            assert!(!root.path().join("Game/a.bin").exists());
            assert_eq!(
                std::fs::read(root.path().join("NotIdentify/a.bin")).unwrap(),
                b"12345678"
            );
            assert!(events.iter().any(|event| matches!(
                event,
                Event::State {
                    status: Status::NotIdentified,
                    ..
                }
            )));
            assert!(!events.iter().any(|event| matches!(
                event,
                Event::State {
                    status: Status::Retrying(_) | Status::Failed(_),
                    ..
                }
            )));
        }
    }

    #[tokio::test]
    async fn even_named_download_stays_in_temporary_directory_until_eof_and_never_overwrites() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ndata")
                .await
                .unwrap();
            started_tx.send(()).unwrap();
            continue_rx.await.unwrap();
            socket.write_all(b"rest").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let request = plan(root.path(), &address, 1, 1);
        let (tx, rx) = mpsc::channel();
        let (result, ()) = tokio::join!(run(request, tx, CancellationToken::new()), async {
            started_rx.await.unwrap();
            // Aguardar o evento de início de gravação sem depender da velocidade do disco.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if rx.try_iter().any(|event| {
                        matches!(
                            event,
                            Event::State {
                                status: Status::Downloading,
                                ..
                            }
                        )
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!root.path().join("Game/file0.bin").exists());
            let temporary: Vec<_> = std::fs::read_dir(root.path().join(staging::DIRECTORY))
                .unwrap()
                .collect();
            assert_eq!(temporary.len(), 1);
            std::fs::write(root.path().join("Game/file0.bin"), b"original").unwrap();
            continue_tx.send(()).unwrap();
        });
        result.unwrap();
        server.await.unwrap();
        assert_eq!(
            std::fs::read(root.path().join("Game/file0.bin")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.path().join("NotIdentify/file0.bin")).unwrap(),
            b"datarest"
        );
        assert_eq!(
            std::fs::read_dir(root.path().join(staging::DIRECTORY))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn unknown_length_uses_final_bytes_to_disambiguate_without_using_line_order() {
        let schema = "Caminho Tamanho_KB CreationTime\n.\\Game\\large\\x.bin 1 21/11/2026 10:00:00\n.\\Game\\small\\x.bin 0,01 21/11/2026 10:00:00";
        let (root, _, events) = unnamed_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=x.bin\r\nConnection: close\r\n\r\n12345678", schema).await;
        assert_eq!(
            std::fs::read(root.path().join("Game/small/x.bin")).unwrap(),
            b"12345678"
        );
        assert!(!root.path().join("Game/large/x.bin").exists());
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Progress { total: None, .. })));
    }

    #[tokio::test]
    async fn ambiguous_download_moves_automatically_to_notidentify_without_another_request() {
        let schema = "Caminho Tamanho_KB CreationTime\n.\\Game\\a\\x.bin 0,01 21/11/2026 10:00:00\n.\\Game\\b\\x.bin 0,01 21/11/2026 10:00:00";
        let (root, structure, events) = unnamed_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=x.bin\r\nContent-Length: 8\r\nConnection: close\r\n\r\n12345678", schema).await;
        assert_eq!(
            std::fs::read(root.path().join("NotIdentify/x.bin")).unwrap(),
            b"12345678"
        );
        assert!(!root.path().join("Game/b/x.bin").exists());
        assert!(events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::NotIdentified,
                ..
            }
        )));
        assert!(staging::load(root.path(), &structure).unwrap().is_empty());
    }

    #[tokio::test]
    async fn queue_recovers_a_registered_complete_without_contacting_its_url() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let mut request = plan(root.path(), "http://127.0.0.1:1", 1, 1);
        request.jobs[0].link.hint = None;
        let mut file = tempfile::Builder::new()
            .suffix(".download")
            .tempfile_in(staging::directory(root.path()).unwrap())
            .unwrap();
        file.write_all(b"data").unwrap();
        file.as_file_mut().sync_all().unwrap();
        std::fs::write(root.path().join(staging::NOT_IDENTIFY), b"blocker").unwrap();
        assert!(matches!(
            Coordinator::new(request.structure.clone())
                .commit(root.path(), &request.jobs[0], file, 4, None)
                .unwrap(),
            Outcome::Pending(_)
        ));
        std::fs::remove_file(root.path().join(staging::NOT_IDENTIFY)).unwrap();
        let (tx, rx) = mpsc::channel();
        run(request, tx, CancellationToken::new()).await.unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::NotIdentified,
                ..
            }
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::Resolving | Status::Downloading | Status::Failed(_),
                ..
            }
        )));
        assert_eq!(
            std::fs::read(root.path().join("NotIdentify/linha-1.download")).unwrap(),
            b"data"
        );
    }

    #[tokio::test]
    async fn segmented_queue_preserves_file_limit_merges_then_organizes_all_files() {
        use crate::segmented::tests::{body, fixture, Mode};
        let root = tempfile::tempdir().unwrap();
        let server = fixture(Mode::Valid, body(32771), Duration::from_millis(80)).await;
        let mut request = plan(
            root.path(),
            &server.url.origin().ascii_serialization(),
            5,
            5,
        );
        request.connections = 5;
        let (tx, rx) = mpsc::channel();
        run(request, tx, CancellationToken::new()).await.unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        for id in 0..5 {
            assert_eq!(
                std::fs::read(root.path().join(format!("Game/file{id}.bin"))).unwrap(),
                server.body
            );
            let merged = events.iter().position(|event| matches!(event, Event::State { id: row, status: Status::Merging } if *row == id)).unwrap();
            let organized = events
                .iter()
                .position(|event| matches!(event, Event::Organized { id: row, .. } if *row == id))
                .unwrap();
            assert!(merged < organized);
        }
        assert!(server.max.load(Ordering::SeqCst) <= 25);
        assert!(server.max.load(Ordering::SeqCst) >= 2);
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen.len(), 30); // 5 testes + 25 partes; não há GET completo
        assert_eq!(seen.iter().filter(|request| request.matched).count(), 25);
        assert_eq!(
            std::fs::read_dir(root.path().join(staging::DIRECTORY))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn segment_fallback_downloads_one_clean_full_copy_without_reusing_mixed_parts() {
        use crate::segmented::tests::{body, fixture, resolved, Mode};
        for mode in [
            Mode::IgnoreRanges,
            Mode::NoEtag,
            Mode::WeakEtag,
            Mode::ChangedEtag,
            Mode::WrongRange,
            Mode::IgnoreAfterProbe,
            Mode::Encoded,
            Mode::PreconditionFailed,
        ] {
            let root = tempfile::tempdir().unwrap();
            let server = fixture(mode, body(4099), Duration::ZERO).await;
            let resolver = Resolver::new().unwrap();
            let (tx, rx) = mpsc::channel();
            let received = receive_download(
                0,
                root.path(),
                &resolver,
                resolved(&server),
                &tx,
                &CancellationToken::new(),
                5,
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read(received.temp.path()).unwrap(),
                server.body,
                "{mode:?}"
            );
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
            assert_eq!(
                server
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request.range.is_none())
                    .count(),
                1
            );
            let events: Vec<_> = rx.try_iter().collect();
            assert!(events.iter().any(|event| matches!(event, Event::Log(message) if message.contains("Usando uma conexão"))));
            let last_parts = events
                .iter()
                .rev()
                .find_map(|event| {
                    if let Event::PartsProgress { parts, .. } = event {
                        Some(parts)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(last_parts.len(), 1);
            assert_eq!(last_parts[0].downloaded, received.bytes);
            assert_eq!(last_parts[0].total, Some(received.bytes));
        }
    }

    #[tokio::test]
    async fn one_connection_never_probes_or_splits_and_invalid_connection_counts_are_rejected() {
        use crate::segmented::tests::{body, fixture, resolved, Mode};
        let root = tempfile::tempdir().unwrap();
        let server = fixture(Mode::Valid, body(1001), Duration::ZERO).await;
        let resolver = Resolver::new().unwrap();
        let (tx, _) = mpsc::channel();
        let received = receive_download(
            0,
            root.path(),
            &resolver,
            resolved(&server),
            &tx,
            &CancellationToken::new(),
            1,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(received.temp.path()).unwrap(), server.body);
        {
            let seen = server.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert!(seen[0].range.is_none());
        }
        for count in [0, 6, usize::MAX] {
            let mut request = plan(
                root.path(),
                &server.url.origin().ascii_serialization(),
                1,
                1,
            );
            request.connections = count;
            let (tx, _) = mpsc::channel();
            assert!(matches!(
                run(request, tx, CancellationToken::new()).await,
                Err(Error::Input(_))
            ));
        }
    }

    #[tokio::test]
    async fn empty_and_tiny_files_reduce_parts_without_changing_content() {
        use crate::segmented::tests::{body, fixture, resolved, Mode};
        for length in 0..=6 {
            let root = tempfile::tempdir().unwrap();
            let server = fixture(Mode::Valid, body(length), Duration::ZERO).await;
            let resolver = Resolver::new().unwrap();
            let (tx, _) = mpsc::channel();
            let received = receive_download(
                0,
                root.path(),
                &resolver,
                resolved(&server),
                &tx,
                &CancellationToken::new(),
                5,
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(received.temp.path()).unwrap(), server.body);
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
            assert_eq!(received.bytes, length as u64);
        }
    }

    #[tokio::test]
    async fn cancellation_of_unnamed_download_removes_incomplete_staging_file() {
        let root = tempfile::tempdir().unwrap();
        let (address, _, server) = server(false, true).await;
        let mut request = plan(root.path(), &address, 1, 1);
        request.jobs[0].link.hint = None;
        let (tx, _) = mpsc::channel();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let task = tokio::spawn(async move { run(request, tx, worker_cancel).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        task.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join(staging::DIRECTORY))
                .unwrap()
                .count(),
            0
        );
        assert!(!root.path().join("Game/file0.bin").exists());
        server.abort();
    }
}
