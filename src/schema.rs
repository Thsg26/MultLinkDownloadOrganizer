use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
};

use serde::Deserialize;

use crate::{
    error::{Error, Result},
    input::{finish_structure, path_key, relative_path, Structure},
    metadata::{FileMetadata, SizeRange},
};

#[derive(Deserialize)]
struct JsonRow {
    #[serde(rename = "Tipo")]
    kind: String,
    #[serde(rename = "Nome")]
    name: String,
    #[serde(rename = "Caminho")]
    path: String,
    #[serde(rename = "Tamanho_Bytes")]
    bytes: Option<u64>,
    #[serde(rename = "Criado_Em")]
    created: String,
}

/// Caminhos absolutos são referências à origem, nunca destinos de escrita.
pub fn parse_json(text: &str) -> Result<Structure> {
    let rows: Vec<JsonRow> = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|error| Error::Input(format!("Schema JSON inválido: {error}")))?;
    let paths: Vec<_> = rows.iter().map(|row| row.path.replace('\\', "/")).collect();
    let absolute = paths.iter().any(|path| is_drive_path(path));
    let prefix = if absolute {
        if paths.iter().any(|path| !is_drive_path(path)) {
            return Err(Error::Input(
                "O JSON mistura caminhos absolutos e relativos.".into(),
            ));
        }
        // A pasta ancestral explicitamente declarada mais curta é a raiz do
        // exportador. Não reduzir apenas ao nome do arquivo ou a um substring.
        let root = rows
            .iter()
            .zip(&paths)
            .filter(|(row, _)| row.kind.eq_ignore_ascii_case("pasta"))
            .map(|(_, path)| path)
            .min_by_key(|path| path.split('/').count())
            .ok_or_else(|| {
                Error::Input("JSON absoluto precisa declarar sua pasta principal.".into())
            })?;
        let (parent, _) = root
            .rsplit_once('/')
            .ok_or_else(|| Error::Input("Raiz de origem inválida.".into()))?;
        let prefix = format!("{parent}/");
        let root_key = root.to_lowercase();
        if paths.iter().any(|path| {
            let key = path.to_lowercase();
            key != root_key && !key.starts_with(&format!("{root_key}/"))
        }) {
            return Err(Error::Input(
                "Os caminhos absolutos não pertencem a uma única pasta principal declarada.".into(),
            ));
        }
        // Validar inclusive o prefixo descartado: nunca aceitar .., ADS ou UNC.
        for path in &paths {
            relative_path(&path[3..])?;
        }
        Some(prefix)
    } else {
        None
    };
    let mut structure = Structure {
        detailed: true,
        ..Structure::default()
    };
    let mut seen = HashSet::new();
    for (index, (row, raw)) in rows.into_iter().zip(paths).enumerate() {
        let fail =
            |message: &str| Error::Input(format!("Entrada {} do JSON: {message}", index + 1));
        let path = if let Some(prefix) = &prefix {
            raw.get(prefix.len()..)
                .ok_or_else(|| fail("prefixo inválido"))?
        } else {
            raw.strip_prefix("./").unwrap_or(&raw)
        };
        let path = relative_path(path).map_err(|error| fail(&error.to_string()))?;
        if path
            .file_name()
            .is_none_or(|name| name.to_string_lossy() != row.name)
        {
            return Err(fail(
                "Nome não corresponde ao último componente de Caminho.",
            ));
        }
        if !seen.insert(path_key(&path)) {
            return Err(fail("caminho repetido."));
        }
        if !valid_json_date(&row.created) {
            return Err(fail("Criado_Em deve usar yyyy-MM-dd HH:mm ou HH:mm:ss."));
        }
        if row.kind.eq_ignore_ascii_case("pasta") {
            if row.bytes.is_some() {
                return Err(fail("pasta deve ter Tamanho_Bytes null."));
            }
            structure.directories.push(path);
        } else if row.kind.eq_ignore_ascii_case("arquivo") {
            let bytes = row
                .bytes
                .ok_or_else(|| fail("arquivo precisa de Tamanho_Bytes inteiro não negativo."))?;
            structure.metadata.insert(
                path_key(&path),
                FileMetadata {
                    size: SizeRange {
                        min: bytes,
                        max: bytes,
                    },
                    size_kib: format!("{}.{:02}", bytes / 1024, (bytes % 1024) * 100 / 1024),
                    created_local: row.created,
                },
            );
            structure.files.push(path);
        } else {
            return Err(fail("Tipo deve ser pasta ou arquivo."));
        }
    }
    finish_structure(structure)
}

fn is_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":/"
}

fn valid_json_date(text: &str) -> bool {
    let Some((date, time)) = text.split_once(' ') else {
        return false;
    };
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
        && valid_date(
            &format!("{}/{}/{}", &date[8..10], &date[5..7], &date[..4]),
            time,
        )
}

pub fn recognizes(text: &str) -> bool {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !is_ignored(line))
        .take(8)
        .any(|line| {
            is_header(line)
                || fields(line).is_some_and(|row| row.date.contains('/') && row.time.contains(':'))
        })
}

pub fn parse(text: &str) -> Result<Structure> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for (line_no, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim();
        if is_ignored(line) || is_header(line) {
            continue;
        }
        let invalid = || {
            Error::Input(format!(
                "Linha {} inválida no schema. Esperado: caminho, tamanho (KB opcional) e dd/MM/aaaa HH:mm ou HH:mm:ss.",
                line_no + 1
            ))
        };
        let Row {
            path,
            kib,
            date,
            time,
        } = fields(line).ok_or_else(invalid)?;
        if !valid_date(date, time) {
            return Err(invalid());
        }
        let size = SizeRange::from_kib(kib)
            .map_err(|error| Error::Input(format!("Linha {} do schema: {error}", line_no + 1)))?;
        let path = path
            .strip_prefix(".\\")
            .or_else(|| path.strip_prefix("./"))
            .unwrap_or(path);
        let path = relative_path(path)
            .map_err(|error| Error::Input(format!("Linha {} do schema: {error}", line_no + 1)))?;
        if !seen.insert(path_key(&path)) {
            return Err(Error::Input(format!(
                "Caminho repetido no schema na linha {}.",
                line_no + 1
            )));
        }
        rows.push((
            path,
            FileMetadata {
                size,
                size_kib: kib.to_owned(),
                created_local: format!("{date} {time}"),
            },
        ));
    }
    // Ser ancestral de outra entrada é evidência de pasta; tamanho zero não é.
    let directories: BTreeSet<PathBuf> = rows
        .iter()
        .flat_map(|(path, _)| {
            path.ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
                .map(PathBuf::from)
        })
        .collect();
    let roots: HashSet<_> = directories
        .iter()
        .filter(|p| p.components().count() == 1)
        .map(|p| path_key(p))
        .collect();
    let mut structure = Structure {
        detailed: true,
        directories: directories.iter().cloned().collect(),
        ..Structure::default()
    };
    for (path, metadata) in rows {
        if directories.contains(&path) {
            continue;
        }
        let root = path
            .components()
            .next()
            .map(|c| PathBuf::from(c.as_os_str()))
            .unwrap_or_default();
        if !roots.is_empty() && !roots.contains(&path_key(&root)) {
            structure.ignored_entries += 1;
            continue;
        }
        structure.metadata.insert(path_key(&path), metadata);
        structure.files.push(path);
    }
    finish_structure(structure)
}

fn is_header(line: &str) -> bool {
    let fields: Vec<_> = line.split_whitespace().collect();
    fields
        .first()
        .is_some_and(|field| field.eq_ignore_ascii_case("Caminho"))
        && fields.iter().any(|field| {
            field.eq_ignore_ascii_case("Tamanho_KB") || field.eq_ignore_ascii_case("Tamanho")
        })
        && fields.iter().any(|field| {
            field.eq_ignore_ascii_case("CreationTime") || field.eq_ignore_ascii_case("Data")
        })
}

fn is_ignored(line: &str) -> bool {
    line.is_empty()
        || line.starts_with('#')
        || line.starts_with("//")
        || line.chars().all(|c| c == '-' || c.is_whitespace())
}

struct Row<'a> {
    path: &'a str,
    kib: &'a str,
    date: &'a str,
    time: &'a str,
}

/// Ler da direita preserva nomes com espaços, acentos e a extensão completa.
fn fields(line: &str) -> Option<Row<'_>> {
    let (line, time) = last_field(line)?;
    let (line, date) = last_field(line)?;
    let (path, size_or_unit) = last_field(line)?;
    let (path, kib) =
        if size_or_unit.eq_ignore_ascii_case("KB") || size_or_unit.eq_ignore_ascii_case("KiB") {
            last_field(path)?
        } else {
            (path, size_or_unit)
        };
    Some(Row {
        path,
        kib,
        date,
        time,
    })
}

fn last_field(line: &str) -> Option<(&str, &str)> {
    let (rest, field) = line.rsplit_once(char::is_whitespace)?;
    Some((rest.trim_end(), field))
}

fn valid_date(date: &str, time: &str) -> bool {
    if date.len() != 10 || !matches!(time.len(), 5 | 8) {
        return false;
    }
    if !date.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 2 | 5) {
            b == b'/'
        } else {
            b.is_ascii_digit()
        }
    }) || !time.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 2 | 5) {
            b == b':'
        } else {
            b.is_ascii_digit()
        }
    }) {
        return false;
    }
    let d: Vec<_> = date
        .split('/')
        .filter_map(|p| p.parse::<u32>().ok())
        .collect();
    let t: Vec<_> = time
        .split(':')
        .filter_map(|p| p.parse::<u32>().ok())
        .collect();
    if d.len() != 3
        || !matches!(t.len(), 2 | 3)
        || !(1..=12).contains(&d[1])
        || !(1..=9999).contains(&d[2])
        || t[0] > 23
        || t[1] > 59
        || t.get(2).is_some_and(|seconds| *seconds > 59)
    {
        return false;
    }
    let leap = d[2].is_multiple_of(4) && (!d[2].is_multiple_of(100) || d[2].is_multiple_of(400));
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    (1..=days[d[1] as usize - 1]).contains(&d[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{parse_structure, read_text};
    use std::path::Path;

    #[test]
    fn upload_json_preserves_exact_sizes_types_spaces_and_accents() {
        let schema = parse_structure(include_str!(
            "../tests/fixtures/upload_download_schema.json"
        ))
        .unwrap();
        assert_eq!(schema.files.len(), 11);
        assert_eq!(schema.directories.len(), 6);
        for path in &schema.files {
            let metadata = &schema.metadata[&path_key(path)];
            assert_eq!(metadata.size.min, metadata.size.max);
            assert_eq!(
                crate::staging::candidates(&schema, None, metadata.size.min),
                vec![path.clone()]
            );
        }
        assert_eq!(
            schema.metadata[&path_key(Path::new(
                "Uploadteste/video/Gravação de Tela 2026-11-11 205414.mp4"
            ))]
                .size
                .min,
            50_576_248
        );
    }

    #[test]
    fn every_game_file_is_identified_by_name_and_exact_final_bytes() {
        let schema = parse_structure(include_str!("../Prompt/download_schema.json")).unwrap();
        for path in &schema.files {
            let size = schema.metadata[&path_key(path)].size;
            assert_eq!(size.min, size.max);
            let name = path.file_name().unwrap().to_string_lossy();
            assert_eq!(
                crate::staging::candidates(&schema, Some(&name), size.min),
                vec![path.clone()],
                "{name}"
            );
        }
    }

    fn json_row(kind: &str, path: &str, bytes: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"Tipo": kind, "Nome": path.rsplit(['/', '\\']).next().unwrap(), "Caminho": path, "Tamanho_Bytes": bytes, "Criado_Em": "2026-11-21 00:17"})
    }

    #[test]
    fn json_distinguishes_empty_directories_empty_files_and_one_byte_files() {
        let rows = serde_json::json!([
            json_row("pasta", ".\\Meu Jogo", serde_json::Value::Null),
            json_row("pasta", ".\\Meu Jogo\\empty.dir", serde_json::Value::Null),
            json_row("arquivo", ".\\Meu Jogo\\empty", 0.into()),
            json_row("arquivo", ".\\Meu Jogo\\one.bin", 1.into())
        ]);
        let schema = parse_structure(&format!("\u{feff}{}", rows)).unwrap();
        assert_eq!(schema.directories.len(), 2);
        assert_eq!(schema.files.len(), 2);
        assert_eq!(
            schema.metadata[&path_key(Path::new("Meu Jogo/empty"))].size,
            SizeRange { min: 0, max: 0 }
        );
        assert!(!schema.metadata[&path_key(Path::new("Meu Jogo/one.bin"))]
            .size
            .contains(0));
    }

    #[test]
    fn json_rejects_unsafe_inconsistent_and_malformed_entries() {
        let valid = json_row("arquivo", ".\\Game\\a.bin", 1.into());
        for path in [
            "..\\a.bin",
            "\\\\server\\share\\a.bin",
            ".\\Game\\a:ads",
            ".\\Game\\CON",
            "C:\\origin\\..\\Game\\a.bin",
        ] {
            let rows = serde_json::json!([
                json_row("pasta", "C:\\origin\\Game", serde_json::Value::Null),
                json_row("arquivo", path, 1.into())
            ]);
            assert!(parse_structure(&rows.to_string()).is_err(), "{path}");
        }
        for (field, value) in [
            ("Tipo", serde_json::json!("executar")),
            ("Nome", serde_json::json!("other.bin")),
            ("Tamanho_Bytes", serde_json::json!(null)),
            ("Tamanho_Bytes", serde_json::json!(-1)),
            ("Tamanho_Bytes", serde_json::json!(1.5)),
            ("Tamanho_Bytes", serde_json::json!("123")),
            ("Criado_Em", serde_json::json!("2026-02-31 10:00")),
        ] {
            let mut row = valid.clone();
            row[field] = value;
            assert!(
                parse_structure(&serde_json::json!([row]).to_string()).is_err(),
                "{field}"
            );
        }
        assert!(parse_structure("[{]")
            .unwrap_err()
            .to_string()
            .contains("JSON"));
        let duplicate = serde_json::json!([valid.clone(), valid.clone()]);
        assert!(parse_structure(&duplicate.to_string()).is_err());
        let two_roots = serde_json::json!([
            json_row("pasta", "C:\\origin\\Game", serde_json::Value::Null),
            json_row("arquivo", "D:\\elsewhere\\Game\\a.bin", 1.into())
        ]);
        assert!(parse_structure(&two_roots.to_string()).is_err());
        let collision = serde_json::json!([
            valid,
            json_row("pasta", ".\\Game\\a.bin", serde_json::Value::Null)
        ]);
        assert!(parse_structure(&collision.to_string()).is_err());
    }

    #[test]
    fn supplied_json_has_108_files_and_exact_one_byte_file() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("Prompt");
        let schema =
            parse_structure(&read_text(&base.join("download_schema.json")).unwrap()).unwrap();
        assert_eq!(schema.files.len(), 108);
        assert_eq!(schema.ignored_entries, 0);
        assert_eq!(schema.metadata.len(), 108);
        assert_eq!(schema.directories.len(), 42);
        assert!(schema
            .files
            .iter()
            .any(|p| path_key(p).ends_with("/pack/story000/data/ttdb/vfx/ttdb_vfx.ttpf")));
        let update = schema
            .metadata
            .iter()
            .find(|(path, _)| path.ends_with("/update.alignmentchunk"))
            .unwrap()
            .1;
        assert_eq!(update.size, SizeRange { min: 1, max: 1 });
        assert_eq!(update.created_local, "2026-11-20 18:17");
        assert!(schema
            .files
            .iter()
            .all(|path| path_key(path).starts_with("gta6.xbox.v1.0.2.1-cyberleek/")));
    }

    #[test]
    fn parses_paths_with_spaces_but_rejects_invalid_data() {
        let schema =
            parse("Caminho Tamanho_KB CreationTime\n.\\Meu Jogo\\a b.bin 1,12 20/11/2026 12:01:02")
                .unwrap();
        assert_eq!(schema.files[0], PathBuf::from("Meu Jogo").join("a b.bin"));
        assert!(parse(".\\Game\\a.bin -1 20/11/2026 12:01:02").is_err());
        assert!(parse(".\\Game\\a.bin 1 31/02/2026 12:01:02").is_err());
        assert!(parse("..\\outside\\a.bin 1 20/11/2026 12:01:02").is_err());
    }

    #[test]
    fn supplied_upload_schema_preserves_spaces_accents_units_and_minute_precision() {
        let text = include_str!("../tests/fixtures/upload_download_schema.txt");
        assert!(recognizes(text));
        let schema = parse_structure(text).unwrap();
        assert!(schema.detailed);
        assert_eq!(schema.files.len(), 11);
        assert_eq!(schema.directories.len(), 6);
        assert_eq!(schema.metadata.len(), 11);
        assert_eq!(schema.ignored_entries, 0);
        let key = path_key(Path::new(
            "Uploadteste/video/Gravação de Tela 2026-11-11 205414.mp4",
        ));
        let video = schema.metadata.get(&key).unwrap();
        assert_eq!(video.size_kib, "49390.87");
        assert_eq!(video.size, SizeRange::from_kib("49390,87").unwrap());
        assert_eq!(video.created_local, "21/11/2026 00:17");
        assert!(schema.files.iter().any(
            |path| path_key(path).ends_with("/texturas/mapa colorido de leonida revelado.png")
        ));
    }

    #[test]
    fn accepts_optional_header_units_separators_and_both_time_precisions() {
        for separator in ["\t", "    "] {
            for size in ["1.12 KB", "1,12 kb", "1.12 KiB", "1,12"] {
                for time in ["12:01", "12:01:02"] {
                    let row = format!(".\\Meu Jogo\\arquivo com acento é.png{separator}{size}{separator}21/11/2026 {time}");
                    for header in [
                        "",
                        "\u{feff}Caminho Tamanho_KB CreationTime\r\n",
                        "Caminho\tTamanho\tData\n",
                    ] {
                        let text = format!("{header}{row}\r\n");
                        assert!(recognizes(&text), "{text}");
                        let schema = parse_structure(&text).unwrap();
                        assert_eq!(
                            schema.files[0],
                            Path::new("Meu Jogo").join("arquivo com acento é.png")
                        );
                        let metadata = schema.metadata.values().next().unwrap();
                        assert!(metadata.size.contains(1152));
                        assert_eq!(metadata.created_local, format!("21/11/2026 {time}"));
                    }
                }
            }
        }
    }

    #[test]
    fn legacy_header_keeps_zero_byte_files_and_ignores_auxiliary_root_files() {
        let text = "Caminho Tamanho_KB CreationTime\n------- ---------- ------------\n.\\Game 0 20/11/2026 10:00:00\n.\\schema.txt 1,12 20/11/2026 10:00:00\n.\\schema_limpo.txt 1,12 20/11/2026 10:00:00\n.\\Game\\empty.bin 0 20/11/2026 10:00:01\n.\\Game\\Caminho com espacos.bin 1,12 20/11/2026 10:00:02";
        let schema = parse_structure(text).unwrap();
        assert_eq!(schema.files.len(), 2);
        assert_eq!(schema.directories.len(), 1);
        assert_eq!(schema.ignored_entries, 2);
        assert!(schema.metadata[&path_key(Path::new("Game/empty.bin"))]
            .size
            .contains(0));
    }

    #[test]
    fn new_schema_rejects_invalid_calendar_time_size_and_missing_fields() {
        for row in [
            ".\\Game\\a.bin\t1 KB\t31/02/2026 12:01",
            ".\\Game\\a.bin\t1 KB\t21/11/2026 24:01",
            ".\\Game\\a.bin\t1 KB\t21/11/2026 12:60",
            ".\\Game\\a.bin\t1 KB\t21/11/2026 12:01:60",
            ".\\Game\\a.bin\t1 KB\t21/11/2026 +1:01",
            ".\\Game\\a.bin\t-1 KB\t21/11/2026 12:01",
            ".\\Game\\a.bin\t1 MB\t21/11/2026 12:01",
            ".\\Game\\a.bin\t1 KB\t21/11/2026",
        ] {
            let text = format!(".\\Game\t0 KB\t21/11/2026 10:00\n{row}");
            assert!(recognizes(&text));
            let error = parse_structure(&text).unwrap_err().to_string();
            assert!(error.contains("Linha 2"), "{error}");
        }
        assert!(parse_structure(".\\Game\\a.bin\t1 KB\t29/02/2024 23:59").is_ok());
        assert!(parse_structure(".\\Game\\a.bin\t1 KB\t29/02/2026 23:59").is_err());
        assert!(parse_structure("..\\outside\\a.bin\t1 KB\t21/11/2026 12:01").is_err());
        assert!(parse_structure("C:\\outside\\a.bin\t1 KB\t21/11/2026 12:01").is_err());
    }

    #[test]
    fn recognition_ignores_leading_comments_but_not_plain_path_lists() {
        let text = format!(
            "{}.\\Game\\a.bin\t1 KB\t21/11/2026 12:01",
            "# comentário\n".repeat(10)
        );
        assert!(recognizes(&text));
        assert!(parse_structure(&text).is_ok());
        let paths = "Game/arquivo KB.bin\nGame/data/arquivo com espaços.png";
        assert!(!recognizes(paths));
        assert_eq!(parse_structure(paths).unwrap().files.len(), 2);
    }
}
