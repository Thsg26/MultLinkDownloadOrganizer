use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::{header, Client, Response};
use scraper::{Html, Selector};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

use crate::{
    browser::{validate_capture, CapturedAccess, EDGE_USER_AGENT},
    error::{Error, Result},
    input::{validate_http_url, Link},
    mapping::Job,
    metadata::RemoteMetadata,
};

pub const USER_AGENT: &str = EDGE_USER_AGENT;
const METADATA_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    VikingFile,
    Gofile,
    Buzzheavier,
}

impl Provider {
    pub fn from_url(url: &Url) -> Option<Self> {
        let host = url.host_str()?.to_ascii_lowercase();
        if domain_matches(&host, "vikingfile.com") {
            Some(Self::VikingFile)
        } else if domain_matches(&host, "gofile.io") {
            Some(Self::Gofile)
        } else if domain_matches(&host, "buzzheavier.com") {
            Some(Self::Buzzheavier)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::VikingFile => "VikingFile",
            Self::Gofile => "Gofile",
            Self::Buzzheavier => "Buzzheavier",
        }
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

pub fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(8))
        .cookie_store(true)
        .build()?)
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub url: Url,
    pub referer: Url,
    pub cookie: Option<String>,
    pub user_agent: Option<String>,
    pub filename: Option<String>,
}

pub struct Resolver {
    pub client: Client,
    gofile_token: Mutex<Option<String>>,
}

impl Resolver {
    pub fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            client: http_client()?,
            gofile_token: Mutex::new(
                std::env::var("MLDO_GOFILE_TOKEN")
                    .ok()
                    .filter(|s| !s.trim().is_empty()),
            ),
        }))
    }

    pub async fn resolve(&self, job: &Job) -> Result<Resolved> {
        if let Some(url) = &job.direct_url {
            if let Some(access) = &job.browser_access {
                validate_capture(access)?;
                if url.as_str() == access.url {
                    return Ok(Resolved {
                        url: url.clone(),
                        referer: Url::parse(&access.referer)
                            .map_err(|_| Error::Host("Origem de navegador inválida.".into()))?,
                        cookie: (!access.cookie.is_empty()).then(|| access.cookie.clone()),
                        user_agent: access.user_agent.clone(),
                        filename: access.filename.clone(),
                    });
                }
            }
            let cookie = if job.link.provider == Provider::Gofile
                && url
                    .host_str()
                    .is_some_and(|host| domain_matches(host, "gofile.io"))
            {
                Some(format!("accountToken={}", self.gofile_session().await?))
            } else {
                None
            };
            return Ok(Resolved {
                url: url.clone(),
                referer: job.link.url.clone(),
                cookie,
                user_agent: None,
                filename: None,
            });
        }
        match job.link.provider {
            Provider::VikingFile => self.viking(&job.link.url).await,
            Provider::Gofile => self.gofile(job).await,
            Provider::Buzzheavier => self.buzzheavier(&job.link.url).await,
        }
    }

    async fn viking(&self, url: &Url) -> Result<Resolved> {
        let response = self
            .client
            .get(url.clone())
            .timeout(METADATA_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        if looks_like_file(&response) {
            return resolved(response.url().clone(), url);
        }
        let base = response.url().clone();
        let html = limited_text(response).await?;
        if let Some(link) = extract_anchor(&html, "a#download-link[href], a[download][href]") {
            let direct = base
                .join(&link)
                .map_err(|_| Error::Host("O VikingFile retornou um endereço inválido.".into()))?;
            if direct != base {
                return resolved(direct, url);
            }
        }
        Err(Error::Authorization("VikingFile: esta página exige captcha, sessão ou liberação no navegador. Use o Navegador ou Acessos para liberar/capturar a URL do botão Download. O captcha é manual.".into()))
    }

    async fn buzzheavier(&self, url: &Url) -> Result<Resolved> {
        let response = self
            .client
            .get(url.clone())
            .timeout(METADATA_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        if looks_like_file(&response) {
            return resolved(response.url().clone(), url);
        }
        let base = response.url().clone();
        let html = limited_text(response).await?;
        if let Some(link) = extract_anchor(&html, "a[download][href], a#download-link[href]") {
            return resolved(
                base.join(&link)
                    .map_err(|_| Error::Host("Endereço de download inválido.".into()))?,
                url,
            );
        }
        let mut endpoint = base.clone();
        endpoint.set_path(&format!("{}/download", base.path().trim_end_matches('/')));
        endpoint.set_query(None);
        let response = self
            .client
            .get(endpoint)
            .header("HX-Request", "true")
            .header("HX-Current-URL", base.as_str())
            .header(header::REFERER, base.as_str())
            .timeout(METADATA_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        if let Some(location) = response
            .headers()
            .get("HX-Redirect")
            .and_then(|v| v.to_str().ok())
        {
            return resolved(
                base.join(location)
                    .map_err(|_| Error::Host("Redirecionamento de download inválido.".into()))?,
                url,
            );
        }
        if let Some(trigger) = response
            .headers()
            .get("HX-Trigger")
            .and_then(|v| v.to_str().ok())
        {
            if let Ok(value) = serde_json::from_str::<Value>(trigger) {
                if let Some(link) = trigger_url(&value) {
                    return resolved(
                        base.join(link)
                            .map_err(|_| Error::Host("Link de download inválido.".into()))?,
                        url,
                    );
                }
            }
        }
        if looks_like_file(&response) {
            return resolved(response.url().clone(), url);
        }
        Err(Error::Authorization("Buzzheavier não liberou o download. O link pode ter expirado ou exigir acesso no navegador. Use o Navegador ou informe uma URL direta em Acessos.".into()))
    }

    async fn gofile(&self, job: &Job) -> Result<Resolved> {
        let url = &job.link.url;
        if url.path().starts_with("/download/") {
            let token = self.gofile_session().await?;
            return Ok(Resolved {
                url: url.clone(),
                referer: url.clone(),
                cookie: Some(format!("accountToken={token}")),
                user_agent: None,
                filename: None,
            });
        }
        let segments: Vec<_> = url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect();
        let ["d", id] = segments.as_slice() else {
            return Err(Error::Host(
                "Use um link Gofile no formato https://gofile.io/d/ID ou uma URL direta.".into(),
            ));
        };
        let token = self.gofile_session().await?;
        let website_token =
            std::env::var("MLDO_GOFILE_WEBSITE_TOKEN").unwrap_or_else(|_| website_token(&token));
        let endpoint = format!("https://api.gofile.io/contents/{id}");
        let response = self
            .client
            .get(endpoint)
            .bearer_auth(&token)
            .header("X-Website-Token", website_token)
            .header("X-BL", "en-US")
            .header(header::REFERER, "https://gofile.io/")
            .header(header::ORIGIN, "https://gofile.io")
            .query(&[
                ("page", "1"),
                ("pageSize", "1000"),
                ("sortField", "name"),
                ("sortDirection", "1"),
            ])
            .timeout(METADATA_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        let data = api_data(&limited_json(response).await?)?;
        let expected = job
            .link
            .hint
            .as_deref()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        let direct = gofile_file(&data, expected)?;
        let filename = if data["type"] == "file" {
            data["name"].as_str().map(str::to_owned)
        } else {
            data["children"]
                .as_object()
                .and_then(|children| {
                    children
                        .values()
                        .find(|child| child["link"].as_str() == Some(direct.as_str()))
                })
                .and_then(|child| child["name"].as_str())
                .map(str::to_owned)
        };
        let direct = Url::parse(&direct)
            .map_err(|_| Error::Host("Gofile retornou uma URL de arquivo inválida.".into()))?;
        validate_http_url(&direct)?;
        // Nunca enviar o token de conta a um domínio arbitrário retornado pela API.
        let cookie = direct
            .host_str()
            .filter(|host| domain_matches(host, "gofile.io"))
            .map(|_| format!("accountToken={token}"));
        Ok(Resolved {
            url: direct,
            referer: Url::parse("https://gofile.io/").expect("URL estática"),
            cookie,
            user_agent: None,
            filename,
        })
    }

    async fn gofile_session(&self) -> Result<String> {
        let mut cached = self.gofile_token.lock().await;
        if cached.is_none() {
            let response = self
                .client
                .post("https://api.gofile.io/accounts")
                .timeout(METADATA_TIMEOUT)
                .send()
                .await?
                .error_for_status()?;
            let data = api_data(&limited_json(response).await?)?;
            *cached = Some(data["token"].as_str().filter(|s| !s.is_empty())
                .ok_or_else(|| Error::Host("Gofile não forneceu uma sessão anônima. Configure MLDO_GOFILE_TOKEN ou tente novamente mais tarde.".into()))?.to_owned());
        }
        Ok(cached.clone().unwrap_or_default())
    }

    pub async fn inspect(
        &self,
        link: &Link,
        direct: Option<Url>,
        browser_access: Option<CapturedAccess>,
    ) -> Result<RemoteMetadata> {
        let browser_name = browser_access
            .as_ref()
            .and_then(|access| access.filename.clone());
        if direct.is_none() && link.provider == Provider::VikingFile {
            let id = link
                .url
                .path_segments()
                .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
                .ok_or_else(|| {
                    Error::Host("VikingFile: identificador de arquivo ausente.".into())
                })?;
            let response = self
                .client
                .post("https://vikingfile.com/api/check-file")
                .form(&[("hash", id)])
                .timeout(METADATA_TIMEOUT)
                .send()
                .await?
                .error_for_status()?;
            let data = limited_json(response).await?;
            let bytes = data["size"]
                .as_u64()
                .or_else(|| data["size"].as_str().and_then(|s| s.parse().ok()));
            if data["exist"] == false || bytes.is_none() {
                return Err(Error::Host("VikingFile não disponibilizou metadados desse arquivo. Use a aba Navegador para liberar o acesso; a identificação acontecerá após baixar.".into()));
            }
            return Ok(RemoteMetadata {
                name: data["name"].as_str().map(str::to_owned),
                bytes,
                last_modified: None,
            });
        }
        let job = Job {
            id: 0,
            link: link.clone(),
            direct_url: direct,
            browser_access,
        };
        let resolved = self.resolve(&job).await?;
        let mut request = self
            .client
            .head(resolved.url)
            .header(header::REFERER, resolved.referer.as_str())
            .header(header::ACCEPT_ENCODING, "identity")
            .timeout(METADATA_TIMEOUT);
        if let Some(cookie) = resolved.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(user_agent) = resolved.user_agent {
            request = request.header(header::USER_AGENT, user_agent);
        }
        let response = request.send().await?.error_for_status()?;
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.contains("html") || content_type.contains("application/json") {
            return Err(Error::Host("A consulta retornou uma página, não o tamanho do arquivo. Libere o acesso na aba Navegador.".into()));
        }
        let bytes = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok());
        if bytes.is_none() {
            return Err(Error::Host(
                "O servidor não informou o tamanho. A identificação usará os bytes finais após o download."
                    .into(),
            ));
        }
        let name = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .and_then(disposition_filename)
            .or(browser_name);
        let last_modified = response
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok(RemoteMetadata {
            name,
            bytes,
            last_modified,
        })
    }
}

pub(crate) fn disposition_filename(value: &str) -> Option<String> {
    let fields: Vec<_> = value.split(';').map(str::trim).collect();
    if let Some(encoded) = fields.iter().find_map(|field| {
        field
            .strip_prefix("filename*=UTF-8''")
            .or_else(|| field.strip_prefix("filename*=utf-8''"))
    }) {
        return percent_encoding::percent_decode_str(encoded)
            .decode_utf8()
            .ok()
            .map(|s| s.into_owned());
    }
    fields.iter().find_map(|field| {
        field
            .strip_prefix("filename=")
            .map(|name| name.trim_matches('"').to_owned())
    })
}

fn resolved(url: Url, referer: &Url) -> Result<Resolved> {
    validate_http_url(&url)?;
    Ok(Resolved {
        url,
        referer: referer.clone(),
        cookie: None,
        user_agent: None,
        filename: None,
    })
}

fn looks_like_file(response: &Response) -> bool {
    response.headers().contains_key(header::CONTENT_DISPOSITION)
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| !s.contains("text/") && !s.contains("json") && !s.contains("xml"))
}

pub async fn limited_text(response: Response) -> Result<String> {
    const MAX: usize = 2 * 1024 * 1024;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX {
            return Err(Error::Host(
                "Resposta do site excedeu o limite de metadados de 2 MiB.".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn limited_json(response: Response) -> Result<Value> {
    serde_json::from_str(&limited_text(response).await?).map_err(|_| Error::Authorization("O site retornou uma página em vez dos dados do arquivo. Pode ser necessário abrir o navegador.".into()))
}

fn api_data(value: &Value) -> Result<Value> {
    if value["status"] != "ok" {
        return Err(Error::Authorization("Gofile: arquivo indisponível, privado, protegido por senha ou sessão recusada. Abra a página e forneça uma URL direta autorizada.".into()));
    }
    value
        .get("data")
        .cloned()
        .ok_or_else(|| Error::Host("Resposta Gofile sem dados.".into()))
}

fn gofile_file(data: &Value, expected: &str) -> Result<String> {
    let mut files = Vec::new();
    if data["type"] == "file" {
        files.push(data);
    }
    if let Some(children) = data["children"].as_object() {
        files.extend(children.values().filter(|child| child["type"] == "file"));
    }
    let matching: Vec<_> = files
        .iter()
        .copied()
        .filter(|child| {
            child["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(expected))
        })
        .collect();
    let chosen = if matching.len() == 1 {
        matching[0]
    } else if files.len() == 1
        && data["children"]
            .as_object()
            .is_none_or(|children| children.len() == 1)
    {
        files[0]
    } else {
        return Err(Error::Host("Gofile: o link deve identificar um arquivo. A pasta contém múltiplos arquivos sem correspondência única com o destino. Use uma URL direta para cada arquivo.".into()));
    };
    chosen["link"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Host("Gofile não disponibilizou o link de download.".into()))
}

fn website_token(token: &str) -> String {
    let bucket = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 14400;
    let input = format!("{USER_AGENT}::en-US::{token}::{bucket}::5d4f7g8sd45fsd");
    format!("{:x}", Sha256::digest(input.as_bytes()))
}

fn extract_anchor(html: &str, selector: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .filter_map(|e| e.value().attr("href"))
        .find(|href| !href.is_empty() && !href.starts_with('#') && !href.starts_with("javascript:"))
        .map(str::to_owned)
}

fn trigger_url(value: &Value) -> Option<&str> {
    value.get("url").and_then(Value::as_str).or_else(|| {
        value
            .get("download")
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_real_links_but_not_placeholder() {
        assert_eq!(
            extract_anchor(
                "<a id='download-link' href='#'>aguarde</a>",
                "a#download-link[href]"
            ),
            None
        );
        assert_eq!(
            extract_anchor(
                "<a id='download-link' href='https://cdn.example/a?x=1&amp;y=2'>Download</a>",
                "a#download-link[href]"
            ),
            Some("https://cdn.example/a?x=1&y=2".into())
        );
    }

    #[test]
    fn gofile_selects_one_matching_file_without_expanding_folder() {
        let data = serde_json::json!({"children": {
            "a": {"type":"file", "name":"a.bin", "link":"https://store.gofile.io/a"},
            "b": {"type":"file", "name":"b.bin", "link":"https://store.gofile.io/b"}
        }});
        assert_eq!(
            gofile_file(&data, "b.bin").unwrap(),
            "https://store.gofile.io/b"
        );
        assert!(gofile_file(&data, "c.bin").is_err());
        assert!(api_data(&serde_json::json!({"status":"error-passwordRequired"})).is_err());
    }

    #[test]
    fn domain_boundaries_are_exact() {
        assert!(domain_matches("store.gofile.io", "gofile.io"));
        assert!(!domain_matches("fakegofile.io", "gofile.io"));
        assert!(!domain_matches("gofile.io.evil.test", "gofile.io"));
    }

    #[tokio::test]
    async fn direct_browser_session_reuses_native_ua_for_head_metadata() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            let mut chunk = [0u8; 1024];
            while !header.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                header.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8_lossy(&header);
            assert!(request.starts_with("HEAD /file "));
            assert!(request.contains("session=authorized"));
            assert!(request.contains("Edg/157.0.0.1"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 1152\r\nContent-Disposition: attachment; filename*=UTF-8''a%20b.bin\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let link = crate::input::parse_links("https://vikingfile.com/f/test#a%20b.bin")
            .unwrap()
            .remove(0);
        let access = CapturedAccess {
            url: url.clone(),
            referer: "https://vikingfile.com/f/test".into(),
            cookie: "session=authorized".into(),
            filename: None,
            user_agent: Some("Mozilla/5.0 Chrome/157.0.0.1 Safari/537.36 Edg/157.0.0.1".into()),
        };
        let metadata = Resolver::new()
            .unwrap()
            .inspect(&link, Some(Url::parse(&url).unwrap()), Some(access))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(metadata.bytes, Some(1152));
        assert_eq!(metadata.name.as_deref(), Some("a b.bin"));
    }
}
