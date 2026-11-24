use std::{sync::mpsc::Sender, thread};

use futures_util::{stream, StreamExt};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    browser::CapturedAccess, error::Result, hosts::Resolver, input::Link, metadata::RemoteMetadata,
};

pub struct Probe {
    pub id: usize,
    pub link: Link,
    pub direct: Option<Url>,
    pub browser_access: Option<CapturedAccess>,
}

pub enum AnalysisEvent {
    Metadata { id: usize, metadata: RemoteMetadata },
    Unavailable { id: usize, message: String },
    Finished,
}

pub fn start(probes: Vec<Probe>, tx: Sender<AnalysisEvent>) -> Result<CancellationToken> {
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    thread::Builder::new().name("mldo-metadata".into()).spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            runtime.block_on(async {
                let resolver = Resolver::new()?;
                stream::iter(probes).for_each_concurrent(3, |probe| {
                    let resolver = resolver.clone(); let tx = tx.clone(); let cancel = worker_cancel.clone();
                    async move {
                        let result = tokio::select! {
                            _ = cancel.cancelled() => return,
                            result = resolver.inspect(&probe.link, probe.direct, probe.browser_access) => result,
                        };
                        let event = match result {
                            Ok(metadata) => AnalysisEvent::Metadata { id: probe.id, metadata },
                            Err(error) => AnalysisEvent::Unavailable { id: probe.id, message: error.to_string() },
                        };
                        let _ = tx.send(event);
                    }
                }).await;
                Ok(())
            })
        }));
        if !matches!(result, Ok(Ok(()))) {
            let _ = tx.send(AnalysisEvent::Unavailable { id: usize::MAX, message: "A consulta de metadados foi interrompida. Tente novamente.".into() });
        }
        let _ = tx.send(AnalysisEvent::Finished);
    })?;
    Ok(cancel)
}
