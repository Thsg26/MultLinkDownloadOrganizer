//! Comando local de identificação/movimentação após EOF. Nunca executa conteúdo.
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, Result},
    input::{path_key, relative_path, Structure},
    mapping::Job,
    safety,
};

pub const DIRECTORY: &str = ".mldo-pendentes";
pub const NOT_IDENTIFY: &str = "NotIdentify";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDownload {
    pub id: usize,
    pub relative: PathBuf,
    pub filename: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
    pub bytes: u64,
    pub candidates: Vec<PathBuf>,
    pub reason: String,
    pub source_key: String,
    pub schema_key: String,
}

pub enum Outcome {
    Organized(PathBuf),
    Unidentified {
        relative: PathBuf,
        reason: String,
    },
    /// Somente falha de disco/permissão mantém um completo na área temporária.
    Pending(PendingDownload),
}

pub struct Coordinator {
    structure: Structure,
    publication: Mutex<()>,
}

impl Coordinator {
    pub fn new(structure: Structure) -> Self {
        Self {
            structure,
            publication: Mutex::new(()),
        }
    }

    pub fn commit(
        &self,
        root: &Path,
        job: &Job,
        temp: tempfile::NamedTempFile,
        bytes: u64,
        filename: Option<String>,
    ) -> Result<Outcome> {
        let relative = temp
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::Input("Área temporária fora do destino.".into()))?
            .to_path_buf();
        if relative.parent() != Some(Path::new(DIRECTORY)) {
            return Err(Error::Input(
                "Download completo fora da área interna.".into(),
            ));
        }
        // Registrar e preservar o completo ANTES de tentar qualquer movimento.
        let (file, _) = temp.keep().map_err(|error| Error::Io(error.error))?;
        drop(file);
        let mut pending = PendingDownload {
            id: job.id,
            relative,
            filename,
            hint: job.link.hint.clone(),
            bytes,
            candidates: Vec::new(),
            reason: String::new(),
            source_key: source_key(job.link.url.as_str()),
            schema_key: schema_key(&self.structure),
        };
        if let Err(error) = save_sidecar(root, &pending) {
            pending.reason = format!("Registro de recuperação indisponível: {error}. ");
        }
        self.organize(root, &pending)
    }

    /// Mesmo comando usado automaticamente após cada download e por --organize.
    pub fn organize(&self, root: &Path, pending: &PendingDownload) -> Result<Outcome> {
        let _guard = self
            .publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let source = validate_pending(root, pending)?;
        if pending.schema_key != schema_key(&self.structure) {
            return Err(Error::Input("O registro pertence a outro schema.".into()));
        }
        let matches = candidates_with_hint(
            &self.structure,
            pending.filename.as_deref(),
            pending.hint.as_deref(),
            pending.bytes,
        );
        let mut reason = match matches.len() {
            0 => "Nome/tamanho final sem correspondência compatível no schema.".to_owned(),
            1 => String::new(),
            _ => "Nome/tamanho final correspondem a mais de um arquivo; não adivinhar pela ordem ou pelos destinos livres.".to_owned(),
        };
        // Não eliminar destinos ocupados para transformar ambiguidade em certeza.
        if matches.len() == 1 {
            let relative = &matches[0];
            match move_complete(root, &source, relative) {
                Ok(()) => {
                    remove_sidecar(root, pending);
                    return Ok(Outcome::Organized(relative.clone()));
                }
                Err(error) => reason = format!("Destino ocupado ou indisponível: {error}"),
            }
        }
        match move_unidentified(root, &source, pending) {
            Ok(relative) => {
                remove_sidecar(root, pending);
                Ok(Outcome::Unidentified { relative, reason })
            }
            Err(error) => {
                let mut record = pending.clone();
                record.candidates = matches;
                record.reason.push_str(&format!(
                    "{reason} Falha ao mover para NotIdentify: {error}. Completo preservado em {}.",
                    record.relative.display()
                ));
                Ok(Outcome::Pending(record))
            }
        }
    }
}

pub fn candidates(structure: &Structure, filename: Option<&str>, bytes: u64) -> Vec<PathBuf> {
    candidates_with_hint(structure, filename, None, bytes)
}

fn candidates_with_hint(
    structure: &Structure,
    filename: Option<&str>,
    hint: Option<&str>,
    bytes: u64,
) -> Vec<PathBuf> {
    let compatible = |path: &&PathBuf| {
        structure
            .metadata
            .get(&path_key(path))
            .is_none_or(|metadata| metadata.size.contains(bytes))
    };
    let normalize = |name: &str| name.replace('\\', "/").to_lowercase();
    let name = filename.filter(|name| !name.is_empty()).map(normalize);
    let hint = hint.filter(|hint| !hint.is_empty()).map(normalize);
    let named: Vec<_> = name
        .as_ref()
        .map(|name| {
            let base = name.rsplit('/').next().unwrap_or(name);
            structure
                .files
                .iter()
                .filter(|path| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                        == base
                })
                .collect()
        })
        .unwrap_or_default();
    let hinted: Vec<_> = hint
        .as_ref()
        .map(|hint| {
            structure
                .files
                .iter()
                .filter(|path| {
                    let key = path_key(path);
                    if hint.contains('/') {
                        key == *hint || key.ends_with(&format!("/{hint}"))
                    } else {
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase()
                            .starts_with(hint)
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let pool = if !named.is_empty() {
        let intersection: Vec<_> = named
            .iter()
            .copied()
            .filter(|path| hinted.contains(path))
            .collect();
        if intersection.is_empty() {
            named
        } else {
            intersection
        }
    } else if !hinted.is_empty() {
        hinted
    } else {
        // Sem nome reconhecível, apenas tamanho conhecido no schema é evidência.
        return structure
            .files
            .iter()
            .filter(|path| {
                structure
                    .metadata
                    .get(&path_key(path))
                    .is_some_and(|metadata| metadata.size.contains(bytes))
            })
            .cloned()
            .collect();
    };
    pool.into_iter().filter(compatible).cloned().collect()
}

pub fn directory(root: &Path) -> Result<PathBuf> {
    let path = safety::checked_path(root, Path::new(DIRECTORY))?;
    fs::create_dir_all(&path)?;
    safety::checked_path(root, Path::new(DIRECTORY))
}

fn move_complete(root: &Path, source: &Path, relative: &Path) -> Result<()> {
    let target = safety::checked_path(root, relative)?;
    if target.try_exists()? {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "arquivo já existe; nada sobrescrito",
        )));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    safety::checked_path(root, relative)?;
    let temporary = tempfile::TempPath::try_from_path(source.to_path_buf())?;
    if let Err(error) = temporary.persist_noclobber(target) {
        let io = error.error;
        let _ = error.path.keep();
        return Err(Error::Io(io));
    }
    Ok(())
}

fn move_unidentified(root: &Path, source: &Path, pending: &PendingDownload) -> Result<PathBuf> {
    let name = pending
        .filename
        .as_deref()
        .or(pending.hint.as_deref())
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .filter(|name| name.chars().count() <= 180)
        .filter(|name| relative_path(name).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("linha-{}.download", pending.id + 1));
    let path = Path::new(&name);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    for index in 0..10_000 {
        let name = if index == 0 {
            name.clone()
        } else {
            format!("{stem} ({index}){extension}")
        };
        let relative = Path::new(NOT_IDENTIFY).join(name);
        match move_complete(root, source, &relative) {
            Ok(()) => return Ok(relative),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Input(
        "Limite de nomes repetidos em NotIdentify atingido.".into(),
    ))
}

fn sidecar(path: &Path) -> PathBuf {
    path.with_extension("download.json")
}

fn save_sidecar(root: &Path, pending: &PendingDownload) -> Result<()> {
    let target = safety::checked_path(root, &sidecar(&pending.relative))?;
    let json =
        serde_json::to_vec_pretty(pending).map_err(|error| Error::Input(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    file.write_all(&json)?;
    file.sync_all()?;
    Ok(())
}

fn remove_sidecar(root: &Path, pending: &PendingDownload) {
    if let Ok(path) = safety::checked_path(root, &sidecar(&pending.relative)) {
        let _ = fs::remove_file(path);
    }
}

pub fn source_key(url: &str) -> String {
    format!("{:x}", Sha256::digest(url.as_bytes()))
}

fn schema_key(structure: &Structure) -> String {
    let mut rows: Vec<_> = structure
        .files
        .iter()
        .map(|path| {
            let key = path_key(path);
            let size = structure
                .metadata
                .get(&key)
                .map(|metadata| format!("{}:{}", metadata.size.min, metadata.size.max))
                .unwrap_or_default();
            format!("{key}\0{size}")
        })
        .collect();
    rows.sort();
    format!("{:x}", Sha256::digest(rows.join("\n").as_bytes()))
}

fn validate_pending(root: &Path, pending: &PendingDownload) -> Result<PathBuf> {
    if pending.relative.components().count() != 2
        || pending.relative.parent() != Some(Path::new(DIRECTORY))
        || pending
            .relative
            .extension()
            .is_none_or(|extension| extension != "download")
    {
        return Err(Error::Input(
            "Arquivo completo fora da área interna do MLDO.".into(),
        ));
    }
    let source = safety::checked_path(root, &pending.relative)?;
    let metadata = fs::metadata(&source)?;
    if !metadata.is_file() || metadata.len() != pending.bytes {
        return Err(Error::Input(
            "O arquivo registrado foi alterado ou está incompleto. Nada será movido.".into(),
        ));
    }
    Ok(source)
}

pub fn load(root: &Path, structure: &Structure) -> Result<Vec<PendingDownload>> {
    let path = safety::checked_path(root, Path::new(DIRECTORY))?;
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let expected_schema = schema_key(structure);
    let mut pending = Vec::new();
    for entry in fs::read_dir(path)?.take(10_000) {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .ends_with(".download.json")
        {
            continue;
        }
        let relative = Path::new(DIRECTORY).join(entry.file_name());
        let Ok(safe) = safety::checked_path(root, &relative) else {
            continue;
        };
        let mut bytes = Vec::new();
        let Ok(file) = fs::File::open(safe) else {
            continue;
        };
        if file.take(256 * 1024 + 1).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if bytes.len() > 256 * 1024 {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<PendingDownload>(&bytes) else {
            continue;
        };
        if record.schema_key == expected_schema
            && sidecar(&record.relative) == relative
            && validate_pending(root, &record).is_ok()
        {
            pending.push(record);
        }
    }
    Ok(pending)
}

/// CLI local: somente completos com registro válido, nunca temporários parciais.
pub fn organize_all(destination: &Path, structure: Structure) -> Result<Vec<Outcome>> {
    let root = safety::prepare(destination, &structure)?;
    let records = load(&root, &structure)?;
    let coordinator = Coordinator::new(structure);
    records
        .iter()
        .map(|record| coordinator.organize(&root, record))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        input::{parse_links, parse_structure},
        mapping::{build_jobs, suggest},
    };

    fn temporary(root: &Path, bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .prefix(".mldo-test-")
            .suffix(".download")
            .tempfile_in(directory(root).unwrap())
            .unwrap();
        file.write_all(bytes).unwrap();
        file.as_file_mut().sync_all().unwrap();
        file
    }

    fn plan(schema: &str) -> (Structure, Vec<Job>) {
        let structure = parse_structure(schema).unwrap();
        let links =
            parse_links("https://vikingfile.com/f/opaque1\nhttps://vikingfile.com/f/opaque2")
                .unwrap();
        let jobs = build_jobs(&links, &structure, &suggest(&links, &structure)).unwrap();
        (structure, jobs)
    }

    #[test]
    fn unique_final_size_identifies_a_file_without_name() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) =
            plan(".\\Game\\a.bin 0,01 21/11/2026 10:00\n.\\Game\\b.bin 1 21/11/2026 10:00");
        let coordinator = Coordinator::new(structure);
        let file = temporary(root.path(), b"12345678");
        assert_eq!(file.path().parent().unwrap().parent(), Some(root.path()));
        assert!(
            matches!(coordinator.commit(root.path(), &jobs[0], file, 8, None).unwrap(), Outcome::Organized(path) if path == Path::new("Game/a.bin"))
        );
        assert_eq!(
            fs::read(root.path().join("Game/a.bin")).unwrap(),
            b"12345678"
        );
    }

    #[test]
    fn ambiguity_is_not_resolved_by_occupied_paths_and_falls_back_outside_main_folder() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) = plan(
            ".\\Game\\a\\x.bin 0,01 21/11/2026 10:00\n.\\Game\\b\\x.bin 0,01 21/11/2026 10:00",
        );
        fs::create_dir_all(root.path().join("Game/a")).unwrap();
        fs::write(root.path().join("Game/a/x.bin"), b"original").unwrap();
        let coordinator = Coordinator::new(structure);
        for job in &jobs {
            assert!(matches!(
                coordinator
                    .commit(
                        root.path(),
                        job,
                        temporary(root.path(), b"12345678"),
                        8,
                        Some("x.bin".into())
                    )
                    .unwrap(),
                Outcome::Unidentified { .. }
            ));
        }
        assert_eq!(
            fs::read(root.path().join("Game/a/x.bin")).unwrap(),
            b"original"
        );
        assert_eq!(
            fs::read(root.path().join("NotIdentify/x.bin")).unwrap(),
            b"12345678"
        );
        assert_eq!(
            fs::read(root.path().join("NotIdentify/x (1).bin")).unwrap(),
            b"12345678"
        );
        assert!(!root.path().join("Game/b/x.bin").exists());
        assert_eq!(
            fs::read_dir(root.path().join(DIRECTORY)).unwrap().count(),
            0
        );
    }

    #[test]
    fn duplicates_unknown_and_incompatible_files_are_preserved_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) = plan("Game/a.bin\nGame/b.bin");
        let coordinator = Coordinator::new(structure);
        for (index, body) in [b"one", b"two"].iter().enumerate() {
            let outcome = coordinator
                .commit(
                    root.path(),
                    &jobs[index],
                    temporary(root.path(), *body),
                    3,
                    Some("a.bin".into()),
                )
                .unwrap();
            assert!(if index == 0 {
                matches!(outcome, Outcome::Organized(_))
            } else {
                matches!(outcome, Outcome::Unidentified { .. })
            });
        }
        assert_eq!(fs::read(root.path().join("Game/a.bin")).unwrap(), b"one");
        assert_eq!(
            fs::read(root.path().join("NotIdentify/a.bin")).unwrap(),
            b"two"
        );
        let (structure, jobs) = plan(".\\Game\\a.bin 1 21/11/2026 10:00");
        let coordinator = Coordinator::new(structure);
        assert!(matches!(
            coordinator
                .commit(
                    root.path(),
                    &jobs[0],
                    temporary(root.path(), b"bad"),
                    3,
                    Some("a.bin".into())
                )
                .unwrap(),
            Outcome::Unidentified { .. }
        ));
        assert_eq!(
            fs::read(root.path().join("NotIdentify/a (1).bin")).unwrap(),
            b"bad"
        );
    }

    #[test]
    fn failed_fallback_is_recoverable_by_automatic_command_without_network() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) = plan("Game/a.bin");
        fs::write(root.path().join(NOT_IDENTIFY), b"blocker").unwrap();
        let coordinator = Coordinator::new(structure.clone());
        let Outcome::Pending(mut record) = coordinator
            .commit(
                root.path(),
                &jobs[0],
                temporary(root.path(), b"data"),
                4,
                Some("unknown.bin".into()),
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(load(root.path(), &structure).unwrap().len(), 1);
        let json = fs::read_to_string(root.path().join(sidecar(&record.relative))).unwrap();
        assert!(!json.contains("https://") && !json.contains("cookie"));
        record.relative = PathBuf::from("../outside.bin");
        assert!(coordinator.organize(root.path(), &record).is_err());
        fs::remove_file(root.path().join(NOT_IDENTIFY)).unwrap();
        let outcomes = organize_all(root.path(), structure.clone()).unwrap();
        assert!(matches!(&outcomes[0], Outcome::Unidentified { .. }));
        assert!(load(root.path(), &structure).unwrap().is_empty());
        assert_eq!(
            fs::read(root.path().join("NotIdentify/unknown.bin")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn modified_complete_and_partial_files_are_never_organized() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) = plan("Game/a.bin");
        fs::write(root.path().join(NOT_IDENTIFY), b"blocker").unwrap();
        let coordinator = Coordinator::new(structure.clone());
        let Outcome::Pending(record) = coordinator
            .commit(
                root.path(),
                &jobs[0],
                temporary(root.path(), b"data"),
                4,
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        fs::write(root.path().join(&record.relative), b"changed!").unwrap();
        assert!(load(root.path(), &structure).unwrap().is_empty());
        assert!(coordinator.organize(root.path(), &record).is_err());
        fs::write(
            root.path().join(DIRECTORY).join("partial.download"),
            b"partial",
        )
        .unwrap();
        assert!(load(root.path(), &structure).unwrap().is_empty());
    }

    #[test]
    fn concurrent_downloads_cannot_publish_the_same_path() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) = plan("Game/a.bin");
        let coordinator = Coordinator::new(structure);
        let outcomes = std::thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .iter()
                .map(|job| {
                    let coordinator = &coordinator;
                    let root = root.path();
                    scope.spawn(move || {
                        coordinator
                            .commit(root, job, temporary(root, b"data"), 4, Some("a.bin".into()))
                            .unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Organized(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Unidentified { .. }))
                .count(),
            1
        );
        assert_eq!(fs::read(root.path().join("Game/a.bin")).unwrap(), b"data");
        assert_eq!(
            fs::read(root.path().join("NotIdentify/a.bin")).unwrap(),
            b"data"
        );
    }

    #[cfg(windows)]
    #[test]
    fn notidentify_junction_never_receives_downloads_and_complete_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let junction = root.path().join(NOT_IDENTIFY);
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(status.status.success());
        let (structure, jobs) = plan("Game/a.bin");
        let result = Coordinator::new(structure)
            .commit(
                root.path(),
                &jobs[0],
                temporary(root.path(), b"data"),
                4,
                Some("unknown.bin".into()),
            )
            .unwrap();
        assert!(matches!(result, Outcome::Pending(_)));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        fs::remove_dir(junction).unwrap();
    }

    #[test]
    fn legacy_record_without_hint_is_automatically_recovered_after_text_reformatting() {
        let root = tempfile::tempdir().unwrap();
        let (structure, jobs) = plan("Caminho Tamanho_KB CreationTime\n.\\Game\\a\\x.bin 0,01 20/11/2026 10:00:17\n.\\Game\\b\\x.bin 0,01 20/11/2026 10:00:19");
        fs::write(root.path().join(NOT_IDENTIFY), b"blocker").unwrap();
        let Outcome::Pending(record) = Coordinator::new(structure)
            .commit(
                root.path(),
                &jobs[0],
                temporary(root.path(), b"12345678"),
                8,
                Some("x.bin".into()),
            )
            .unwrap()
        else {
            panic!()
        };
        let sidecar = root.path().join(sidecar(&record.relative));
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("hint");
        fs::write(sidecar, serde_json::to_vec(&json).unwrap()).unwrap();
        fs::remove_file(root.path().join(NOT_IDENTIFY)).unwrap();
        let current = parse_structure(".\\Game\\b\\x.bin\t0.01 KB\t20/11/2026 10:00\n.\\Game\\a\\x.bin\t0.01 KB\t20/11/2026 10:00").unwrap();
        assert!(matches!(
            &organize_all(root.path(), current).unwrap()[0],
            Outcome::Unidentified { .. }
        ));
        assert_eq!(
            fs::read(root.path().join("NotIdentify/x.bin")).unwrap(),
            b"12345678"
        );
    }
}
