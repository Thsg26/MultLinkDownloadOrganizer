use super::*;
use std::sync::{atomic::AtomicUsize, Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Valid,
    UnevenProgress,
    IgnoreRanges,
    NoEtag,
    WeakEtag,
    UnknownTotal,
    WrongRange,
    WrongTotal,
    WrongLength,
    ChangedEtag,
    IgnoreAfterProbe,
    Encoded,
    Truncated,
    Oversized,
    PreconditionFailed,
    Unauthorized,
    RateLimited,
}

#[derive(Debug)]
pub(crate) struct Seen {
    pub range: Option<(u64, u64)>,
    pub matched: bool,
    pub session: bool,
    pub has_cookie: bool,
}

pub(crate) struct Fixture {
    pub url: url::Url,
    pub body: Vec<u8>,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    pub max: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(crate) fn body(length: usize) -> Vec<u8> {
    (0..length)
        .map(|i| (i.wrapping_mul(71).wrapping_add(17) % 256) as u8)
        .collect()
}

pub(crate) async fn fixture(mode: Mode, body: Vec<u8>, delay: Duration) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/file", listener.local_addr().unwrap())).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let max = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn({
        let body = Arc::new(body.clone());
        let seen = seen.clone();
        let max = max.clone();
        async move {
            while let Ok((socket, _)) = listener.accept().await {
                let body = body.clone();
                let seen = seen.clone();
                let max = max.clone();
                let active = active.clone();
                tokio::spawn(serve(socket, mode, body, seen, max, active, delay));
            }
        }
    });
    Fixture {
        url,
        body,
        seen,
        max,
        handle,
    }
}

async fn serve(
    socket: TcpStream,
    mode: Mode,
    body: Arc<Vec<u8>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    max: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    delay: Duration,
) {
    let mut socket = socket;
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
        let Ok(count) = socket.read(&mut buffer).await else {
            return;
        };
        if count == 0 || request.len() > 16384 {
            return;
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let text = String::from_utf8_lossy(&request).to_lowercase();
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
    };
    let range = field("range: bytes=")
        .and_then(|text| text.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?)));
    let matched = field("if-match:") == Some("\"v1\"") && field("if-range:") == Some("\"v1\"");
    let session = field("cookie:") == Some("session=local")
        && field("user-agent:") == Some("mldo-local-test")
        && field("accept-encoding:") == Some("identity")
        && field("referer:").is_some();
    seen.lock().unwrap().push(Seen {
        range,
        matched,
        session,
        has_cookie: field("cookie:").is_some(),
    });
    let probe = range == Some((0, 0)) && !matched;
    let part = range.is_some() && !probe;
    if mode == Mode::Unauthorized || mode == Mode::RateLimited {
        let status = if mode == Mode::Unauthorized {
            "403 Forbidden"
        } else {
            "429 Too Many Requests"
        };
        let _ = socket
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        return;
    }
    if part && mode == Mode::PreconditionFailed {
        let _ = socket.write_all(b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return;
    }
    let ranged =
        range.is_some() && mode != Mode::IgnoreRanges && !(part && mode == Mode::IgnoreAfterProbe);
    let mut headers = String::new();
    let mut data = if ranged {
        let (start, end) = range.unwrap();
        if start > end || end >= body.len() as u64 {
            let _ = socket.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return;
        }
        let first = if part && mode == Mode::WrongRange {
            start + 1
        } else {
            start
        };
        let total = if probe && mode == Mode::UnknownTotal {
            "*".to_owned()
        } else {
            (body.len() + usize::from(part && mode == Mode::WrongTotal)).to_string()
        };
        headers.push_str(&format!("Content-Range: bytes {first}-{end}/{total}\r\n"));
        body[start as usize..=end as usize].to_vec()
    } else {
        body.as_ref().clone()
    };
    if part && mode == Mode::ChangedEtag {
        data.fill(0xaa);
    }
    if mode != Mode::NoEtag {
        let tag = if mode == Mode::WeakEtag {
            "W/\"v1\""
        } else if part && mode == Mode::ChangedEtag {
            "\"v2\""
        } else {
            "\"v1\""
        };
        headers.push_str(&format!("ETag: {tag}\r\n"));
    }
    if part && mode == Mode::Encoded {
        headers.push_str("Content-Encoding: gzip\r\n");
    }
    if part && mode == Mode::Oversized {
        data.push(0xff);
    } else {
        headers.push_str(&format!(
            "Content-Length: {}\r\n",
            data.len() + usize::from(part && mode == Mode::WrongLength)
        ));
    }
    if part && mode == Mode::Truncated {
        data.pop();
    }
    let status = if ranged {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let headers = format!("HTTP/1.1 {status}\r\n{headers}Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=real.bin\r\nConnection: close\r\n\r\n");
    if socket.write_all(headers.as_bytes()).await.is_err() {
        return;
    }
    if part {
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(count, Ordering::SeqCst);
    }
    let middle = data.len().div_ceil(2);
    let first = socket.write_all(&data[..middle]).await;
    if part && first.is_ok() {
        let delay = if mode == Mode::UnevenProgress && range.is_some_and(|(start, _)| start > 0) {
            Duration::ZERO
        } else {
            delay
        };
        tokio::time::sleep(delay).await;
    }
    let _ = socket.write_all(&data[middle..]).await;
    if part {
        active.fetch_sub(1, Ordering::SeqCst);
    }
    let _ = socket.shutdown().await;
}

pub(crate) fn resolved(server: &Fixture) -> Resolved {
    Resolved {
        url: server.url.clone(),
        referer: server.url.clone(),
        cookie: Some("session=local".into()),
        user_agent: Some("mldo-local-test".into()),
        filename: None,
    }
}

#[test]
fn ranges_cover_every_byte_once_in_order_even_for_large_and_tiny_files() {
    for total in [0, 1, 2, 4, 5, 6, 17, 1001, 20_022_508_672, u64::MAX] {
        for count in 1..=5 {
            let plan = ranges(total, count);
            assert_eq!(plan.len() as u64, total.min(count as u64));
            let mut next = 0;
            for range in plan {
                assert_eq!(range.start, next);
                assert!(range.len() > 0);
                next = range.end + 1;
            }
            assert_eq!(next, total);
        }
    }
    for text in [
        "bytes 0-0/*",
        "bytes 1-0/5",
        "bytes 0-5/5",
        "bytes +0-1/2",
        "bytes 0-1/0",
        "0-1/2",
        "bytes 0-1/2 extra",
        "bytes 0-1/18446744073709551616",
    ] {
        assert!(content_range(text).is_none(), "{text}");
    }
}

#[tokio::test]
async fn two_to_five_connections_reconstruct_identical_bytes_and_aggregate_progress() {
    for count in 2..=5 {
        let root = tempfile::tempdir().unwrap();
        let server = fixture(Mode::Valid, body(128 * 1024 + 3), Duration::from_millis(40)).await;
        let resolver = Resolver::new().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let Attempt::Ready(received) = receive(
            0,
            root.path(),
            &resolver,
            &resolved(&server),
            &tx,
            &CancellationToken::new(),
            count,
        )
        .await
        .unwrap() else {
            panic!("Range válido não deve usar fallback")
        };
        assert_eq!(std::fs::read(received.temp.path()).unwrap(), server.body);
        assert_eq!(received.bytes, server.body.len() as u64);
        assert_eq!(received.filename.as_deref(), Some("real.bin"));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        assert!(server.max.load(Ordering::SeqCst) <= count);
        assert!(server.max.load(Ordering::SeqCst) >= 2);
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen.len(), count + 1);
        assert!(seen
            .iter()
            .all(|request| request.range.is_some() && request.session));
        assert_eq!(seen.iter().filter(|request| request.matched).count(), count);
        let events: Vec<_> = rx.try_iter().collect();
        let snapshots: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let Event::PartsProgress { parts, .. } = event {
                    Some(parts)
                } else {
                    None
                }
            })
            .collect();
        assert!(!snapshots.is_empty());
        for snapshot in &snapshots {
            assert_eq!(snapshot.len(), count);
            assert!(snapshot
                .iter()
                .all(|part| part.total.is_some_and(|total| part.downloaded <= total)));
        }
        assert!(snapshots
            .last()
            .unwrap()
            .iter()
            .all(|part| Some(part.downloaded) == part.total));
        for pair in events.windows(2) {
            if let Event::PartsProgress { id, parts } = &pair[0] {
                assert!(
                    matches!(&pair[1], Event::Progress { id: next, downloaded, total, .. }
                    if next == id && *downloaded == parts.iter().map(|part| part.downloaded).sum::<u64>() && *total == parts.iter().map(|part| part.total).sum())
                );
            }
        }
        assert!(events.iter().any(|event| matches!(
            event,
            Event::State {
                status: Status::Merging,
                ..
            }
        )));
        let progress: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let Event::Progress {
                    downloaded, total, ..
                } = event
                {
                    assert_eq!(*total, Some(server.body.len() as u64));
                    Some(*downloaded)
                } else {
                    None
                }
            })
            .collect();
        assert!(progress.windows(2).all(|window| window[0] <= window[1]));
        assert_eq!(progress.last().copied(), Some(server.body.len() as u64));
        drop(received);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn progress_reports_a_slow_part_independently_of_finished_parts() {
    let root = tempfile::tempdir().unwrap();
    let server = fixture(
        Mode::UnevenProgress,
        body(65541),
        Duration::from_millis(900),
    )
    .await;
    let resolver = Resolver::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(matches!(
        receive(
            7,
            root.path(),
            &resolver,
            &resolved(&server),
            &tx,
            &CancellationToken::new(),
            5
        )
        .await
        .unwrap(),
        Attempt::Ready(_)
    ));
    let snapshots: Vec<_> = rx
        .try_iter()
        .filter_map(|event| {
            if let Event::PartsProgress { id: 7, parts } = event {
                Some(parts)
            } else {
                None
            }
        })
        .collect();
    assert!(snapshots.iter().any(|parts| parts[0].downloaded > 0
        && Some(parts[0].downloaded) < parts[0].total
        && parts[1..]
            .iter()
            .all(|part| Some(part.downloaded) == part.total)));
    for pair in snapshots.windows(2) {
        for index in 0..5 {
            assert!(pair[0][index].downloaded <= pair[1][index].downloaded);
        }
    }
}

#[tokio::test]
async fn unsupported_or_inconsistent_responses_leave_no_parts_to_merge() {
    for mode in [
        Mode::IgnoreRanges,
        Mode::NoEtag,
        Mode::WeakEtag,
        Mode::UnknownTotal,
        Mode::WrongRange,
        Mode::WrongTotal,
        Mode::WrongLength,
        Mode::ChangedEtag,
        Mode::IgnoreAfterProbe,
        Mode::Encoded,
        Mode::Oversized,
        Mode::PreconditionFailed,
    ] {
        let root = tempfile::tempdir().unwrap();
        let server = fixture(mode, body(4099), Duration::from_millis(5)).await;
        let resolver = Resolver::new().unwrap();
        let (tx, _) = std::sync::mpsc::channel();
        assert!(
            matches!(
                receive(
                    0,
                    root.path(),
                    &resolver,
                    &resolved(&server),
                    &tx,
                    &CancellationToken::new(),
                    5
                )
                .await
                .unwrap(),
                Attempt::Fallback(_)
            ),
            "{mode:?}"
        );
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            0,
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn transport_and_access_failures_are_not_treated_as_success_or_range_fallback() {
    for mode in [Mode::Truncated, Mode::Unauthorized, Mode::RateLimited] {
        let root = tempfile::tempdir().unwrap();
        let server = fixture(mode, body(4099), Duration::from_millis(5)).await;
        let resolver = Resolver::new().unwrap();
        let (tx, _) = std::sync::mpsc::channel();
        let result = receive(
            0,
            root.path(),
            &resolver,
            &resolved(&server),
            &tx,
            &CancellationToken::new(),
            5,
        )
        .await;
        assert!(result.is_err(), "{mode:?}");
        if mode == Mode::Unauthorized {
            assert!(matches!(result, Err(Error::Http(403))));
        }
        if mode == Mode::RateLimited {
            assert!(matches!(result, Err(Error::Http(429))));
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn cancellation_cleans_all_concurrent_parts_without_publishing() {
    let root = tempfile::tempdir().unwrap();
    let server = fixture(Mode::Valid, body(256 * 1024), Duration::from_secs(2)).await;
    let resolver = Resolver::new().unwrap();
    let (tx, _) = std::sync::mpsc::channel();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        task_cancel.cancel();
    });
    assert!(matches!(
        receive(
            0,
            root.path(),
            &resolver,
            &resolved(&server),
            &tx,
            &cancel,
            5
        )
        .await,
        Err(Error::Cancelled)
    ));
    cancel_task.await.unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn html_split_across_tiny_parts_is_rejected_after_reassembly() {
    let root = tempfile::tempdir().unwrap();
    let server = fixture(Mode::Valid, b"<html".to_vec(), Duration::ZERO).await;
    let resolver = Resolver::new().unwrap();
    let (tx, _) = std::sync::mpsc::channel();
    assert!(matches!(
        receive(
            0,
            root.path(),
            &resolver,
            &resolved(&server),
            &tx,
            &CancellationToken::new(),
            5
        )
        .await,
        Err(Error::Host(_))
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn cancelling_during_merge_removes_assembled_file_and_all_parts() {
    let root = tempfile::tempdir().unwrap();
    let server = fixture(Mode::Valid, body(512 * 1024), Duration::ZERO).await;
    let resolver = Resolver::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = CancellationToken::new();
    let resolved = resolved(&server);
    let (result, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(
            receive(0, root.path(), &resolver, &resolved, &tx, &cancel, 5),
            async {
                loop {
                    if rx.try_iter().any(|event| {
                        matches!(
                            event,
                            Event::State {
                                status: Status::Merging,
                                ..
                            }
                        )
                    }) {
                        cancel.cancel();
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn probe_redirect_does_not_forward_explicit_cookie_to_another_origin_for_parts() {
    let root = tempfile::tempdir().unwrap();
    let target = fixture(Mode::Valid, body(4099), Duration::ZERO).await;
    let mut target_url = target.url.clone();
    target_url.set_host(Some("localhost")).unwrap();
    let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_url =
        url::Url::parse(&format!("http://{}/redirect", source.local_addr().unwrap())).unwrap();
    let redirect = tokio::spawn(async move {
        let (mut socket, _) = source.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while !bytes.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    });
    let mut resolved = resolved(&target);
    resolved.url = source_url;
    let resolver = Resolver::new().unwrap();
    let (tx, _) = std::sync::mpsc::channel();
    let result = receive(
        0,
        root.path(),
        &resolver,
        &resolved,
        &tx,
        &CancellationToken::new(),
        3,
    )
    .await
    .unwrap();
    assert!(matches!(result, Attempt::Ready(_)));
    redirect.await.unwrap();
    let seen = target.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|request| !request.has_cookie));
}

#[tokio::test]
async fn merge_rejects_missing_reordered_truncated_and_same_size_modified_parts() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let make = |range: ByteRange, bytes: &[u8]| {
        let mut temp = tempfile::Builder::new()
            .suffix(".part")
            .tempfile_in(root.path())
            .unwrap();
        temp.write_all(bytes).unwrap();
        Part {
            temp,
            range,
            hash: Sha256::digest(bytes).into(),
        }
    };
    for scenario in 0..4 {
        let mut parts = vec![
            make(
                ByteRange {
                    start: 0,
                    end: 2,
                    total: 6,
                },
                b"abc",
            ),
            make(
                ByteRange {
                    start: 3,
                    end: 5,
                    total: 6,
                },
                b"def",
            ),
        ];
        let original: Vec<_> = parts
            .iter()
            .map(|part| part.temp.path().to_path_buf())
            .collect();
        match scenario {
            0 => {
                parts.pop();
            }
            1 => parts.swap(0, 1),
            2 => std::fs::write(parts[0].temp.path(), b"ab").unwrap(),
            _ => std::fs::write(parts[0].temp.path(), b"xyz").unwrap(),
        }
        let result = merge(root.path(), &parts, 6).await;
        assert!(result.is_err());
        // Windows pode enumerar brevemente nomes de exclusão .TMP*.PART.tmp;
        // verificar os arquivos do aplicativo, não contar entradas alheias.
        let assembled: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "download")
            })
            .collect();
        assert!(assembled.is_empty(), "cenário {scenario}: {assembled:?}");
        drop(parts);
        assert!(original.iter().all(|path| !path.exists()));
    }
}
