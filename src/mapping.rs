use std::collections::HashSet;
use url::Url;

use crate::{
    browser::{validate_capture, CapturedAccess},
    error::{Error, Result},
    input::{path_key, validate_http_url, Link, Structure},
    metadata::RemoteMetadata,
};

#[derive(Clone, Default)]
pub struct Assignment {
    pub target: Option<usize>,
    pub candidates: Vec<usize>,
    pub direct_url: String,
    pub browser_access: Option<CapturedAccess>,
    pub remote: Option<RemoteMetadata>,
    pub basis: String,
}

#[derive(Clone)]
pub struct Job {
    pub id: usize,
    pub link: Link,
    pub direct_url: Option<Url>,
    pub browser_access: Option<CapturedAccess>,
}

impl Job {
    pub fn label(&self) -> String {
        format!("Linha {} · organizar após o download", self.link.line)
    }
}

pub fn suggest(links: &[Link], structure: &Structure) -> Vec<Assignment> {
    let mut used = HashSet::new();
    links
        .iter()
        .map(|link| {
            let hint = link.hint.as_deref().unwrap_or("").to_lowercase();
            let mut candidates: Vec<usize> = structure
                .files
                .iter()
                .enumerate()
                .filter_map(|(index, path)| {
                    let key = path_key(path);
                    let matched = if hint.contains('/') {
                        key == hint || key.ends_with(&format!("/{hint}"))
                    } else {
                        !hint.is_empty()
                            && path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_lowercase()
                                == hint
                    };
                    matched.then_some(index)
                })
                .collect();
            let exact = candidates.len() == 1;
            if candidates.is_empty() && !hint.is_empty() {
                candidates = structure
                    .files
                    .iter()
                    .enumerate()
                    .filter_map(|(index, path)| {
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase()
                            .starts_with(&hint)
                            .then_some(index)
                    })
                    .collect();
            }
            let target = if exact && used.insert(candidates[0]) {
                Some(candidates[0])
            } else {
                None
            };
            Assignment {
                target,
                candidates,
                direct_url: String::new(),
                basis: if target.is_some() {
                    "Nome/caminho único".into()
                } else if link.hint.is_none() {
                    "Nome ausente: identificar e organizar após o download".into()
                } else {
                    "Aguardando metadados ou confirmação".into()
                },
                ..Assignment::default()
            }
        })
        .collect()
}

/// Usar tamanho remoto somente se houver uma correspondência única dentro dos
/// candidatos de nome/caminho. Nunca usar CreationTime local como data remota.
pub fn apply_metadata(
    links: &[Link],
    structure: &Structure,
    assignments: &mut [Assignment],
    id: usize,
    remote: RemoteMetadata,
) -> bool {
    if id >= assignments.len() || id >= links.len() {
        return false;
    }
    assignments[id].remote = Some(remote.clone());
    if assignments[id].target.is_some() {
        return false;
    } // preservar sugestão já disponível (nunca usada como destino pré-download)
      // Sem nome na entrada, não fixar uma pasta por HEAD/API: a decisão deve
      // usar o arquivo efetivamente recebido e seu tamanho final, após o EOF.
    if links[id].hint.is_none() {
        return false;
    }
    let Some(bytes) = remote.bytes else {
        return false;
    };
    let name = remote.name.as_deref().map(str::to_lowercase);
    if assignments[id].candidates.is_empty() && name.is_none() {
        return false;
    }
    let pool = if assignments[id].candidates.is_empty() {
        (0..structure.files.len()).collect::<Vec<_>>()
    } else {
        assignments[id].candidates.clone()
    };
    let used: HashSet<_> = assignments.iter().filter_map(|a| a.target).collect();
    let compatible: Vec<_> = pool
        .into_iter()
        .filter(|index| {
            let path = &structure.files[*index];
            let same_name = name.as_ref().is_none_or(|name| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
                    == *name
            });
            let name_evidence = name.is_some() || links[id].hint.is_some();
            !used.contains(index)
                && same_name
                && name_evidence
                && structure
                    .metadata
                    .get(&path_key(path))
                    .is_some_and(|metadata| metadata.size.contains(bytes))
        })
        .collect();
    if compatible.len() == 1 {
        assignments[id].target = Some(compatible[0]);
        assignments[id].basis = "Nome + tamanho remoto compatível único".into();
        true
    } else {
        false
    }
}

pub fn build_jobs(
    links: &[Link],
    _structure: &Structure,
    assignments: &[Assignment],
) -> Result<Vec<Job>> {
    if links.len() != assignments.len() {
        return Err(Error::Input(
            "Recarregue os arquivos para atualizar a organização.".into(),
        ));
    }
    links
        .iter()
        .zip(assignments)
        .enumerate()
        .map(|(id, (link, assignment))| {
            let direct_url = if assignment.direct_url.trim().is_empty() {
                None
            } else {
                let mut url = Url::parse(assignment.direct_url.trim()).map_err(|_| {
                    Error::Input(format!("Link direto inválido na linha {}.", link.line))
                })?;
                validate_http_url(&url)?;
                url.set_fragment(None);
                Some(url)
            };
            let browser_access = assignment
                .browser_access
                .as_ref()
                .filter(|access| {
                    direct_url
                        .as_ref()
                        .is_some_and(|url| url.as_str() == access.url)
                })
                .cloned();
            if let Some(access) = &browser_access {
                validate_capture(access)?;
            }
            Ok(Job {
                id,
                link: link.clone(),
                direct_url,
                browser_access,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{parse_links, parse_structure};

    #[test]
    fn name_suggestions_and_duplicates_never_block_downloads() {
        let structure = parse_structure("Game/a/x.bin\nGame/b/x.bin\nGame/c/y.bin").unwrap();
        let links = parse_links(
            "https://gofile.io/d/1#y.bin\nhttps://gofile.io/d/2#x.bin\nhttps://gofile.io/d/3#x.bin",
        )
        .unwrap();
        let mut assignments = suggest(&links, &structure);
        assert_eq!(assignments[0].target, Some(2));
        assert_eq!(assignments[1].target, None);
        assert_eq!(assignments[1].candidates, vec![0, 1]);
        assert!(build_jobs(&links, &structure, &assignments).is_ok());
        assignments[1].target = Some(0);
        assignments[2].target = Some(1);
        assert!(build_jobs(&links, &structure, &assignments).is_ok());
        assignments[2].target = Some(0);
        assert!(build_jobs(&links, &structure, &assignments).is_ok());
    }

    #[test]
    fn full_path_and_prefix_are_only_pre_download_suggestions() {
        let structure = parse_structure("Game/a/x.bin\nGame/b/x.bin\nGame/long-name.xml").unwrap();
        let links = parse_links("https://vikingfile.com/f/1#a/x.bin\nhttps://vikingfile.com/f/2#b/x.bin\nhttps://vikingfile.com/f/3#long-name").unwrap();
        let assignments = suggest(&links, &structure);
        assert_eq!(assignments[0].target, Some(0));
        assert_eq!(assignments[1].target, Some(1));
        assert_eq!(assignments[2].candidates, vec![2]);
        assert!(assignments[2].target.is_none());
    }

    #[test]
    fn all_108_supplied_links_start_without_destination_confirmations() {
        use crate::input::read_text;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Prompt");
        let links = parse_links(&read_text(&root.join("links.txt")).unwrap()).unwrap();
        let structure =
            parse_structure(&read_text(&root.join("download_schema.json")).unwrap()).unwrap();
        let assignments = suggest(&links, &structure);
        assert_eq!(
            assignments.iter().filter(|a| a.target.is_none()).count(),
            11
        );
        assert_eq!(
            assignments.iter().filter(|a| a.target.is_some()).count(),
            97
        );
        let jobs = build_jobs(&links, &structure, &assignments).unwrap();
        assert_eq!(jobs.len(), 108);
        assert!(jobs
            .iter()
            .all(|job| job.label().contains("após o download")));
        assert_eq!(
            build_jobs(&links[..3], &structure, &assignments[..3])
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn schema_sizes_resolve_duplicates_and_truncated_name_without_using_dates() {
        use crate::input::read_text;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Prompt");
        let links = parse_links(&read_text(&root.join("links.txt")).unwrap()).unwrap();
        let structure =
            parse_structure(&read_text(&root.join("download_schema.json")).unwrap()).unwrap();
        let mut assignments = suggest(&links, &structure);
        // Metadados simulados, não uma consulta aos arquivos remotos do usuário.
        let pending: Vec<_> = assignments
            .iter()
            .enumerate()
            .filter(|(_, a)| a.target.is_none())
            .map(|(i, a)| (i, a.candidates.clone()))
            .collect();
        let mut chosen = HashSet::new();
        for (id, candidates) in pending {
            let target = candidates.into_iter().find(|i| chosen.insert(*i)).unwrap();
            let path = &structure.files[target];
            let range = structure.metadata[&path_key(path)].size;
            let metadata = RemoteMetadata {
                name: Some(path.file_name().unwrap().to_string_lossy().into_owned()),
                bytes: Some((range.min + range.max) / 2),
                last_modified: Some("data diferente da local".into()),
            };
            assert!(apply_metadata(
                &links,
                &structure,
                &mut assignments,
                id,
                metadata
            ));
        }
        assert!(assignments.iter().all(|a| a.target.is_some()));
        assert!(build_jobs(&links, &structure, &assignments).is_ok());
    }

    #[test]
    fn refuses_size_collisions_and_preserves_existing_suggestions() {
        let schema = "Caminho Tamanho_KB CreationTime\n.\\Game\\a\\x.bin 1 20/11/2026 10:00:00\n.\\Game\\b\\x.bin 1 20/11/2026 11:00:00";
        let structure = parse_structure(schema).unwrap();
        let links =
            parse_links("https://gofile.io/d/1#x.bin\nhttps://gofile.io/d/2#x.bin").unwrap();
        let mut assignments = suggest(&links, &structure);
        let remote = RemoteMetadata {
            name: Some("x.bin".into()),
            bytes: Some(1024),
            last_modified: None,
        };
        assert!(!apply_metadata(
            &links,
            &structure,
            &mut assignments,
            0,
            remote.clone()
        ));
        assignments[0].target = Some(1);
        assert!(!apply_metadata(
            &links,
            &structure,
            &mut assignments,
            0,
            remote
        ));
        assert_eq!(assignments[0].target, Some(1));
    }

    #[test]
    fn links_without_names_can_start_without_destinations() {
        let structure = parse_structure("Game/data/a.bin\nGame/b.bin").unwrap();
        let links =
            parse_links("https://vikingfile.com/f/opaque1\nhttps://gofile.io/d/opaque2").unwrap();
        let mut assignments = suggest(&links, &structure);
        assert!(!apply_metadata(
            &links,
            &structure,
            &mut assignments,
            0,
            RemoteMetadata {
                name: Some("a.bin".into()),
                bytes: Some(1024),
                last_modified: None
            }
        ));
        assert!(assignments[0].target.is_none());
        let jobs = build_jobs(&links, &structure, &assignments).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].label().contains("após o download"));
    }
}
