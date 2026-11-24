//! Entrada local sem shell, sem rede e sem comandos fornecidos pelo schema.
use std::{ffi::OsString, io::Write, path::PathBuf};

use crate::{
    error::{Error, Result},
    input,
    staging::{self, Outcome},
};

pub fn run(args: &[OsString]) -> Result<PathBuf> {
    let usage = || {
        Error::Input(
            "Uso: MLDO.exe --organize --schema arquivo.json --destination pasta-base".into(),
        )
    };
    if args.first().is_none_or(|arg| arg != "--organize") {
        return Err(usage());
    }
    let mut schema = None;
    let mut destination = None;
    let (pairs, remainder) = args[1..].as_chunks::<2>();
    for pair in pairs {
        if pair[0] == "--schema" && schema.is_none() {
            schema = Some(PathBuf::from(&pair[1]));
        } else if pair[0] == "--destination" && destination.is_none() {
            destination = Some(PathBuf::from(&pair[1]));
        } else {
            return Err(usage());
        }
    }
    if !remainder.is_empty() {
        return Err(usage());
    }
    let schema = schema.ok_or_else(usage)?;
    let destination = destination.ok_or_else(usage)?;
    let structure = input::parse_structure(&input::read_text(&schema)?)?;
    let outcomes = staging::organize_all(&destination, structure)?;
    let mut report = tempfile::Builder::new()
        .prefix("mldo-organize-")
        .suffix(".log")
        .tempfile_in(&destination)?;
    writeln!(
        report,
        "MLDO — organização local de {} completos registrados",
        outcomes.len()
    )?;
    let mut failed = false;
    for outcome in outcomes {
        match outcome {
            Outcome::Organized(path) => writeln!(report, "Organizado: {}", path.display())?,
            Outcome::Unidentified { relative, reason } => {
                writeln!(report, "NotIdentify: {} — {reason}", relative.display())?
            }
            Outcome::Pending(record) => {
                failed = true;
                writeln!(
                    report,
                    "Preservado: {} — {}",
                    record.relative.display(),
                    record.reason
                )?;
            }
        }
    }
    report.as_file_mut().sync_all()?;
    let (_, path) = report.keep().map_err(|error| Error::Io(error.error))?;
    if failed {
        return Err(Error::Input(format!(
            "Há completos que não puderam ser movidos. Relatório: {}",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_duplicated_or_unrecognized_options() {
        for args in [
            vec!["--organize"],
            vec!["--organize", "--schema"],
            vec!["--organize", "--execute", "bad.ps1"],
            vec!["--organize", "--schema", "a", "--schema", "b"],
        ] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            assert!(run(&args).is_err());
        }
    }
}
