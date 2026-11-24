use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use crate::{
    error::{Error, Result},
    input::{relative_path, Structure},
};

fn is_redirect(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

pub fn checked_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let safe = relative_path(&relative.to_string_lossy())?;
    let mut path = root.to_path_buf();
    for component in safe.components() {
        let Component::Normal(name) = component else {
            return Err(Error::Input("Caminho inseguro.".into()));
        };
        path.push(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_redirect(&metadata) => {
                return Err(Error::Input(format!(
                    "Link simbólico/junção não permitido no destino: {}",
                    path.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub fn prepare(root: &Path, structure: &Structure) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)?;
    if is_redirect(&metadata) || !metadata.is_dir() {
        return Err(Error::Input(
            "Escolha uma pasta de destino real, não um link simbólico ou junção.".into(),
        ));
    }
    let root = root.canonicalize()?;
    if structure
        .files
        .iter()
        .chain(&structure.directories)
        .any(|path| {
            path.components().next().is_some_and(|part| {
                part.as_os_str()
                    .eq_ignore_ascii_case(crate::staging::DIRECTORY)
                    || part
                        .as_os_str()
                        .eq_ignore_ascii_case(crate::staging::NOT_IDENTIFY)
            })
        })
    {
        return Err(Error::Input(
            "As pastas .mldo-pendentes e NotIdentify são reservadas ao MLDO na pasta-base.".into(),
        ));
    }
    let probe = tempfile::Builder::new()
        .prefix(".mldo-permissao-")
        .tempfile_in(&root)?;
    drop(probe);
    // A árvore é criada antes das transferências, mas destinos de arquivos
    // ocupados não bloqueiam a fila: a publicação os envia a NotIdentify.
    for directory in &structure.directories {
        let path = match checked_path(&root, directory) {
            Ok(path) => path,
            Err(Error::Io(_)) => continue, // destino ocupado/inacessível → fallback após EOF
            Err(error) => return Err(error), // nunca tolerar junções/traversal
        };
        if !path.try_exists().unwrap_or(true) {
            let _ = fs::create_dir_all(&path);
        }
        if let Err(Error::Input(message)) = checked_path(&root, directory) {
            return Err(Error::Input(message));
        }
    }
    crate::staging::directory(&root)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::parse_structure;

    #[test]
    fn creates_empty_folders_and_existing_files_do_not_block_queue() {
        let root = tempfile::tempdir().unwrap();
        let structure = parse_structure("Game/empty/\nGame/data/a.bin").unwrap();
        prepare(root.path(), &structure).unwrap();
        assert!(root.path().join("Game/empty").is_dir());
        fs::write(root.path().join("Game/data/a.bin"), b"original").unwrap();
        assert!(prepare(root.path(), &structure).is_ok());
        assert_eq!(
            fs::read(root.path().join("Game/data/a.bin")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn checked_path_rejects_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        assert!(checked_path(root.path(), Path::new("../escape")).is_err());
    }

    #[test]
    fn main_folder_collisions_do_not_block_staging_but_internal_names_are_reserved() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Game"), b"original").unwrap();
        let structure = parse_structure("Game/data/a.bin").unwrap();
        assert!(prepare(root.path(), &structure).is_ok());
        assert!(root.path().join(crate::staging::DIRECTORY).is_dir());
        assert_eq!(fs::read(root.path().join("Game")).unwrap(), b"original");
        for schema in ["NotIdentify/a.bin", ".mldo-pendentes/a.bin"] {
            let structure = parse_structure(schema).unwrap();
            assert!(prepare(root.path(), &structure).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_junction_without_touching_external_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel.txt"), b"preservar").unwrap();
        let junction = root.path().join("redirect");
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Não foi possível criar a junção de teste"
        );
        assert!(checked_path(root.path(), Path::new("redirect/file.bin")).is_err());
        fs::remove_dir(junction).unwrap();
        assert_eq!(
            fs::read(outside.path().join("sentinel.txt")).unwrap(),
            b"preservar"
        );
    }
}
