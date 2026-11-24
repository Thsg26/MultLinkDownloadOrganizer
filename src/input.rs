use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use percent_encoding::percent_decode_str;
use url::Url;

use crate::{
    error::{Error, Result},
    hosts::Provider,
};

#[derive(Debug, Clone)]
pub struct Link {
    pub line: usize,
    pub url: Url,
    pub hint: Option<String>,
    pub provider: Provider,
}

#[derive(Debug, Clone, Default)]
pub struct Structure {
    pub directories: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub metadata: HashMap<String, crate::metadata::FileMetadata>,
    pub detailed: bool,
    pub ignored_entries: usize,
}

pub fn read_text(path: &Path) -> Result<String> {
    use std::io::Read;
    const MAX: u64 = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX {
        return Err(Error::Input(
            "O arquivo de texto excede o limite de 8 MiB.".into(),
        ));
    }
    decode_text(&bytes)
}

pub fn decode_text(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        if !bytes.len().is_multiple_of(2) {
            return Err(Error::Input("Arquivo UTF-16 incompleto.".into()));
        }
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        return String::from_utf16(&units)
            .map_err(|_| Error::Input("Texto UTF-16 inválido.".into()));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_owned());
    }
    Ok(encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned())
}

pub fn parse_links(text: &str) -> Result<Vec<Link>> {
    let mut links = Vec::new();
    for (index, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let mut url = Url::parse(line)
            .map_err(|_| Error::Input(format!("Link inválido na linha {}.", index + 1)))?;
        validate_http_url(&url).map_err(|_| {
            Error::Input(format!(
                "URL inválida na linha {}. Use http/https, sem credenciais.",
                index + 1
            ))
        })?;
        let provider = Provider::from_url(&url).ok_or_else(|| {
            Error::Input(format!(
                "Site não suportado na linha {}. Use VikingFile, Gofile ou Buzzheavier.",
                index + 1
            ))
        })?;
        let hint = url
            .fragment()
            .map(|f| {
                percent_decode_str(f)
                    .decode_utf8_lossy()
                    .trim()
                    .replace('\\', "/")
            })
            .filter(|f| !f.is_empty());
        url.set_fragment(None);
        links.push(Link {
            line: index + 1,
            url,
            hint,
            provider,
        });
    }
    if links.is_empty() {
        return Err(Error::Input("Nenhum link encontrado no arquivo.".into()));
    }
    Ok(links)
}

pub fn validate_http_url(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Input(
            "Use uma URL HTTP/HTTPS válida, sem usuário ou senha.".into(),
        ));
    }
    Ok(())
}

pub fn relative_path(text: &str) -> Result<PathBuf> {
    let text = text.replace('\\', "/");
    if text.is_empty() || text.starts_with('/') || text.ends_with('/') {
        return Err(Error::Input(format!("Caminho relativo inválido: {text}")));
    }
    let mut result = PathBuf::new();
    for component in text.split('/') {
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        if component.is_empty()
            || matches!(component, "." | "..")
            || reserved
            || component.ends_with(['.', ' '])
            || component
                .chars()
                .any(|c| c.is_control() || "<>:\"|?*".contains(c))
        {
            return Err(Error::Input(format!(
                "Nome de pasta/arquivo inseguro ou incompatível com Windows: {component}"
            )));
        }
        result.push(component);
    }
    Ok(result)
}

pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

pub fn parse_structure(text: &str) -> Result<Structure> {
    let json = text.trim_start_matches('\u{feff}').trim_start();
    if json.starts_with(['[', '{']) {
        return crate::schema::parse_json(json);
    }
    if crate::schema::recognizes(text) {
        return crate::schema::parse(text);
    }
    let normalized = text
        .trim_start_matches('\u{feff}')
        .replace('│', "|")
        .replace('├', "+")
        .replace('└', "\\")
        .replace('─', "-");
    let tree = normalized
        .lines()
        .any(|l| l.contains("+---") || l.contains("\\---"));
    let mut result = Structure::default();
    if tree {
        let mut stack: Vec<String> = Vec::new();
        for (line_no, line) in normalized.lines().enumerate() {
            let branch = line.find("+---").or_else(|| line.find("\\---"));
            if let Some(column) = branch {
                if !line[..column].chars().all(|c| c == ' ' || c == '|') || column % 4 != 0 {
                    return Err(Error::Input(format!(
                        "Indentação inválida na linha {} da árvore.",
                        line_no + 1
                    )));
                }
                let depth = column / 4;
                if depth > stack.len() {
                    return Err(Error::Input(format!(
                        "Pasta sem pai na linha {}.",
                        line_no + 1
                    )));
                }
                let name = line[column + 4..].trim_end();
                let component = relative_path(name)?;
                if component.components().count() != 1 {
                    return Err(Error::Input("Nome de pasta inválido na árvore.".into()));
                }
                stack.truncate(depth);
                stack.push(name.to_owned());
                result.directories.push(relative_path(&stack.join("/"))?);
            } else if !stack.is_empty() {
                let column = line.chars().take_while(|c| *c == ' ' || *c == '|').count();
                let name = line.get(column..).unwrap_or("").trim_end();
                if name.is_empty() {
                    continue;
                }
                // tree /F usa uma célula de quatro colunas extra antes dos arquivos.
                if column < 8 || column % 4 != 0 {
                    continue;
                }
                let depth = column / 4 - 1;
                if depth > stack.len() {
                    return Err(Error::Input(format!(
                        "Arquivo sem pasta na linha {}.",
                        line_no + 1
                    )));
                }
                stack.truncate(depth);
                let file = relative_path(name)?;
                if file.components().count() != 1 {
                    return Err(Error::Input("Nome de arquivo inválido na árvore.".into()));
                }
                result
                    .files
                    .push(relative_path(&format!("{}/{}", stack.join("/"), name))?);
            }
        }
    } else {
        for line in normalized
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            if line.ends_with(['/', '\\']) {
                result
                    .directories
                    .push(relative_path(line.trim_end_matches(['/', '\\']))?);
            } else {
                result.files.push(relative_path(line)?);
            }
        }
    }
    finish_structure(result)
}

pub(crate) fn finish_structure(mut result: Structure) -> Result<Structure> {
    if result.files.is_empty() {
        return Err(Error::Input("Nenhum arquivo encontrado na estrutura. Use um schema com caminho/tamanho/data, tree /F /A ou caminhos relativos, um por linha.".into()));
    }
    let mut file_keys = BTreeSet::new();
    for file in &result.files {
        if !file_keys.insert(path_key(file)) {
            return Err(Error::Input(format!(
                "Arquivo repetido na estrutura: {}",
                file.display()
            )));
        }
    }
    let mut dirs = BTreeSet::new();
    for path in result.directories.iter().chain(result.files.iter()) {
        for parent in path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            dirs.insert(parent.to_path_buf());
        }
    }
    dirs.extend(result.directories);
    if dirs.iter().any(|p| file_keys.contains(&path_key(p))) {
        return Err(Error::Input(
            "Um caminho foi definido como pasta e como arquivo.".into(),
        ));
    }
    // Colisões entre pastas que diferem apenas em maiúsculas não são portáveis.
    let mut keys = BTreeSet::new();
    for dir in &dirs {
        if !keys.insert(path_key(dir)) {
            return Err(Error::Input(
                "Pastas diferem apenas em maiúsculas/minúsculas. Corrija a estrutura.".into(),
            ));
        }
    }
    result.directories = dirs.into_iter().collect();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_supplied_schema_without_outside_manifests() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("Prompt");
        let structure =
            parse_structure(&read_text(&base.join("download_schema.json")).unwrap()).unwrap();
        let links = parse_links(&read_text(&base.join("links.txt")).unwrap()).unwrap();
        assert_eq!(links.len(), 108);
        assert_eq!(structure.files.len(), 108);
        assert!(structure
            .files
            .iter()
            .any(|p| path_key(p).ends_with("pack/story000/data/ttdb/vfx/ttdb_vfx.ttpf")));
        assert!(!structure
            .files
            .iter()
            .any(|p| path_key(p).ends_with("schema_detalhado.txt")
                || path_key(p).ends_with("schema_limpo.txt")));
    }

    #[test]
    fn unicode_windows_tree_and_empty_directories() {
        let parsed = parse_structure(
            "C:.\n│   estrutura.txt\n└───Game\n    │   root.bin\n    ├───empty\n    └───data\n            child.bin\n",
        )
        .unwrap();
        assert_eq!(parsed.files.len(), 2);
        assert!(parsed
            .directories
            .contains(&PathBuf::from("Game").join("empty")));
    }

    #[test]
    fn rejects_traversal_and_windows_reserved_names() {
        for value in [
            "../bad",
            "C:/bad",
            "/root",
            "a//b",
            "a/CON.txt",
            "a/ok.",
            "a/aux",
            "a/file:stream",
            "a/LPT1",
        ] {
            assert!(relative_path(value).is_err(), "{value}");
        }
        assert!(relative_path("Game/data/file.ttpf").is_ok());
    }

    #[test]
    fn rejects_collisions_and_invalid_hosts() {
        assert!(parse_structure("Game/a.bin\nGame/A.bin").is_err());
        assert!(parse_structure("Game/a\nGame/a/b.bin").is_err());
        assert!(parse_links("https://vikingfile.com.evil.test/f/id").is_err());
        assert!(parse_links("file:///a").is_err());
        assert!(parse_links("https://user:password@gofile.io/d/abc").is_err());
    }

    #[test]
    fn decodes_boms_and_legacy_text() {
        assert_eq!(decode_text(&[0xff, 0xfe, b'a', 0]).unwrap(), "a");
        assert_eq!(decode_text(&[0xef, 0xbb, 0xbf, b'a']).unwrap(), "a");
        assert_eq!(decode_text(&[0xe7]).unwrap(), "ç");
    }
}
