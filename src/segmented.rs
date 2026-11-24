//! Intervalos HTTP somente com representação estável (ETag forte), nunca por
//! simples anúncio de Accept-Ranges. Partes e mesclagem ficam na mesma base.
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use futures_util::{future::try_join_all, StreamExt};
use reqwest::{header, Response, StatusCode};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    download::{
        file_request, filename_and_type, reject_html, report_progress, Event, PartProgress,
        Received, Status,
    },
    error::{Error, Result},
    hosts::{Resolved, Resolver},
};

pub(crate) enum Attempt {
    Ready(Received),
    Fallback(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
    total: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

fn ranges(total: u64, count: usize) -> Vec<ByteRange> {
    if total == 0 || !(1..=5).contains(&count) {
        return Vec::new();
    }
    let count = (count as u64).min(total);
    let mut start = 0;
    (0..count)
        .map(|index| {
            let length = total / count + u64::from(index < total % count);
            let range = ByteRange {
                start,
                end: start + length - 1,
                total,
            };
            start += length;
            range
        })
        .collect()
}

fn content_range(text: &str) -> Option<ByteRange> {
    let (span, total) = text.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let number = |text: &str| {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse::<u64>().ok()
    };
    let range = ByteRange {
        start: number(start)?,
        end: number(end)?,
        total: number(total)?,
    };
    (range.start <= range.end && range.end < range.total).then_some(range)
}

fn single_header(response: &Response, name: header::HeaderName) -> Option<&str> {
    if response.headers().get_all(&name).iter().count() != 1 {
        return None;
    }
    response.headers().get(name)?.to_str().ok()
}

fn strong_etag(response: &Response) -> Option<String> {
    let tag = single_header(response, header::ETAG)?;
    (tag.len() >= 2
        && tag.starts_with('"')
        && tag.ends_with('"')
        && tag.as_bytes()[1..tag.len() - 1]
            .iter()
            .all(|byte| *byte >= 0x21 && *byte != b'"' && *byte != 0x7f))
    .then(|| tag.to_owned())
}

fn identity_encoding(response: &Response) -> bool {
    response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value
                .to_str()
                .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
        })
}

pub(crate) async fn receive(
    id: usize,
    parent: &Path,
    resolver: &Resolver,
    resolved: &Resolved,
    tx: &Sender<Event>,
    cancel: &CancellationToken,
    connections: usize,
) -> Result<Attempt> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = receive_inner(id, parent, resolver, resolved, tx, cancel, connections) => result,
    }
}

async fn receive_inner(
    id: usize,
    parent: &Path,
    resolver: &Resolver,
    resolved: &Resolved,
    tx: &Sender<Event>,
    cancel: &CancellationToken,
    connections: usize,
) -> Result<Attempt> {
    // GET de um byte comprova suporte real sem depender de HEAD/Accept-Ranges.
    let mut probe = file_request(resolver, resolved)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await?;
    if matches!(
        probe.status(),
        StatusCode::OK
            | StatusCode::BAD_REQUEST
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::RANGE_NOT_SATISFIABLE
            | StatusCode::NOT_IMPLEMENTED
    ) {
        return Ok(Attempt::Fallback(
            "O servidor não aceitou o teste de intervalos HTTP.",
        ));
    }
    probe = probe.error_for_status()?;
    if probe.status() != StatusCode::PARTIAL_CONTENT {
        return Ok(Attempt::Fallback("Resposta de intervalos não é HTTP 206."));
    }
    let filename = filename_and_type(&probe, resolved, None)?;
    let Some(range) = single_header(&probe, header::CONTENT_RANGE)
        .and_then(content_range)
        .filter(|range| range.start == 0 && range.end == 0)
    else {
        return Ok(Attempt::Fallback(
            "Content-Range ausente ou inválido no teste.",
        ));
    };
    if !identity_encoding(&probe) || probe.content_length().is_some_and(|length| length != 1) {
        return Ok(Attempt::Fallback(
            "Representação ou tamanho do teste incompatível.",
        ));
    }
    let Some(etag) = strong_etag(&probe) else {
        return Ok(Attempt::Fallback(
            "Sem ETag forte para impedir mistura de versões do arquivo.",
        ));
    };
    let plan = ranges(range.total, connections);
    if plan.len() < 2 {
        return Ok(Attempt::Fallback(
            "Arquivo pequeno demais para mais de uma parte.",
        ));
    }
    let mut pinned = resolved.clone();
    pinned.url = probe.url().clone();
    // O redirecionamento do probe não autoriza reenviar cookies explícitos a
    // outra origem. O cookie jar continua aplicando suas regras de domínio.
    if pinned.url.origin() != resolved.url.origin() {
        pinned.cookie = None;
    }
    pinned.filename = filename.clone();
    let mut probe_bytes = 0;
    while let Some(chunk) = probe.chunk().await? {
        probe_bytes += chunk.len();
        if probe_bytes > 1 {
            return Ok(Attempt::Fallback(
                "O teste retornou mais bytes que o intervalo solicitado.",
            ));
        }
    }
    if probe_bytes != 1 {
        return Ok(Attempt::Fallback("O teste de intervalo veio incompleto."));
    }
    drop(probe);
    let _ = tx.send(Event::Log(format!(
        "Linha {}: dividindo em {} partes com ETag estável e tamanho total de {} bytes.",
        id + 1,
        plan.len(),
        range.total
    )));
    let _ = tx.send(Event::State {
        id,
        status: Status::Downloading,
    });
    if let Some(filename) = &filename {
        let _ = tx.send(Event::Identified {
            id,
            filename: filename.clone(),
        });
    }
    let progress: Vec<_> = plan.iter().map(|_| AtomicU64::new(0)).collect();
    let snapshot = || {
        plan.iter()
            .zip(&progress)
            .map(|(range, bytes)| PartProgress {
                downloaded: bytes.load(Ordering::Relaxed),
                total: Some(range.len()),
            })
            .collect::<Vec<_>>()
    };
    report_progress(id, snapshot(), 0.0, tx);
    let transfers = try_join_all(
        plan.iter()
            .zip(&progress)
            .map(|(range, bytes)| receive_part(parent, resolver, &pinned, &etag, *range, bytes)),
    );
    tokio::pin!(transfers);
    let mut ticker = tokio::time::interval(Duration::from_millis(350));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sample_bytes = 0;
    let mut sample_time = Instant::now();
    let parts = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = &mut transfers => match result {
                Ok(parts) => break parts,
                Err(PartError::Incompatible(reason)) => return Ok(Attempt::Fallback(reason)),
                Err(PartError::Fatal(error)) => return Err(error),
            },
            _ = ticker.tick() => {
                let parts = snapshot();
                let downloaded: u64 = parts.iter().map(|part| part.downloaded).sum();
                let speed = (downloaded - sample_bytes) as f64 / sample_time.elapsed().as_secs_f64().max(0.001);
                report_progress(id, parts, speed, tx);
                sample_bytes = downloaded;
                sample_time = Instant::now();
            }
        }
    };
    // As futures de rede terminam antes da mesclagem: nenhum pedido extra aqui.
    report_progress(id, snapshot(), 0.0, tx);
    let _ = tx.send(Event::State {
        id,
        status: Status::Merging,
    });
    let temp = merge(parent, &parts, range.total).await?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let _ = tx.send(Event::Log(format!(
        "Linha {}: {} partes mescladas em ordem, hashes locais e {} bytes verificados.",
        id + 1,
        parts.len(),
        range.total
    )));
    Ok(Attempt::Ready(Received {
        temp,
        bytes: range.total,
        filename,
    }))
}

enum PartError {
    Incompatible(&'static str),
    Fatal(Error),
}
impl From<Error> for PartError {
    fn from(error: Error) -> Self {
        Self::Fatal(error)
    }
}
impl From<std::io::Error> for PartError {
    fn from(error: std::io::Error) -> Self {
        Self::Fatal(error.into())
    }
}
impl From<reqwest::Error> for PartError {
    fn from(error: reqwest::Error) -> Self {
        Self::Fatal(error.into())
    }
}

struct Part {
    temp: tempfile::NamedTempFile,
    range: ByteRange,
    hash: [u8; 32],
}

async fn receive_part(
    parent: &Path,
    resolver: &Resolver,
    resolved: &Resolved,
    etag: &str,
    range: ByteRange,
    progress: &AtomicU64,
) -> std::result::Result<Part, PartError> {
    let response = file_request(resolver, resolved)
        .header(
            header::RANGE,
            format!("bytes={}-{}", range.start, range.end),
        )
        .header(header::IF_MATCH, etag)
        .header(header::IF_RANGE, etag)
        .send()
        .await?;
    if matches!(
        response.status(),
        StatusCode::OK | StatusCode::PRECONDITION_FAILED | StatusCode::RANGE_NOT_SATISFIABLE
    ) {
        return Err(PartError::Incompatible(
            "O servidor deixou de aceitar intervalos ou o arquivo mudou.",
        ));
    }
    let response = response.error_for_status()?;
    if response.status() != StatusCode::PARTIAL_CONTENT
        || response.url() != &resolved.url
        || strong_etag(&response).as_deref() != Some(etag)
        || single_header(&response, header::CONTENT_RANGE).and_then(content_range) != Some(range)
        || !identity_encoding(&response)
        || response
            .content_length()
            .is_some_and(|length| length != range.len())
    {
        return Err(PartError::Incompatible(
            "Parte com URL, versão, intervalo, codificação ou tamanho divergente.",
        ));
    }
    filename_and_type(&response, resolved, None)?;
    let temp = tempfile::Builder::new()
        .prefix(".mldo-parte-")
        .suffix(".part")
        .tempfile_in(parent)?;
    // I/O em blocos na thread de downloads, nunca na interface. Sem operações
    // de disco destacadas que possam manter handles abertos após cancelamento.
    let mut file = temp.reopen()?;
    let mut chunks = response.bytes_stream();
    let mut bytes = 0u64;
    let mut hash = Sha256::new();
    let mut prefix = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if chunk.len() as u64 > range.len() - bytes {
            return Err(PartError::Incompatible(
                "Uma parte ultrapassou seu intervalo; não mesclar.",
            ));
        }
        if range.start == 0 && prefix.len() < 512 {
            prefix.extend_from_slice(&chunk[..chunk.len().min(512 - prefix.len())]);
            reject_html(&prefix)?;
        }
        for block in chunk.chunks(128 * 1024) {
            file.write_all(block)?;
            hash.update(block);
            bytes += block.len() as u64;
            progress.fetch_add(block.len() as u64, Ordering::Relaxed);
            tokio::task::yield_now().await;
        }
    }
    if bytes != range.len() {
        return Err(PartError::Incompatible(
            "Uma parte terminou incompleta; não mesclar.",
        ));
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    Ok(Part {
        temp,
        range,
        hash: hash.finalize().into(),
    })
}

async fn merge(parent: &Path, parts: &[Part], total: u64) -> Result<tempfile::NamedTempFile> {
    let temp = tempfile::Builder::new()
        .prefix(".mldo-mesclado-")
        .suffix(".download")
        .tempfile_in(parent)?;
    let mut output = temp.reopen()?;
    let mut written = 0u64;
    let mut prefix = Vec::new();
    let mut buffer = vec![0u8; 128 * 1024];
    let invalid = || {
        Error::Host(
            "Verificação das partes/mesclagem falhou. Nada será publicado ou organizado.".into(),
        )
    };
    for part in parts {
        if part.range.start != written
            || part.range.total != total
            || part.temp.as_file().metadata()?.len() != part.range.len()
        {
            return Err(invalid());
        }
        let mut input = part.temp.reopen()?;
        let mut bytes = 0u64;
        let mut hash = Sha256::new();
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            if count as u64 > part.range.len() - bytes {
                return Err(invalid());
            }
            if prefix.len() < 512 {
                prefix.extend_from_slice(&buffer[..count.min(512 - prefix.len())]);
                reject_html(&prefix)?;
            }
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
            bytes += count as u64;
            tokio::task::yield_now().await;
        }
        if bytes != part.range.len() || <[u8; 32]>::from(hash.finalize()) != part.hash {
            return Err(invalid());
        }
        written = written.checked_add(bytes).ok_or_else(invalid)?;
    }
    if written != total {
        return Err(invalid());
    }
    output.flush()?;
    output.sync_all()?;
    if output.metadata()?.len() != total {
        return Err(invalid());
    }
    drop(output);
    Ok(temp)
}

#[cfg(test)]
pub(crate) mod tests;
