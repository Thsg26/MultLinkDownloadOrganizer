use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Intervalo de bytes compatível com KiB arredondado a duas casas no PowerShell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeRange {
    pub min: u64,
    pub max: u64,
}

impl SizeRange {
    pub fn from_kib(text: &str) -> Result<Self> {
        let normalized = if text.contains(',') {
            text.replace('.', "").replace(',', ".")
        } else {
            text.to_owned()
        };
        let mut parts = normalized.split('.');
        let integer = parts.next().unwrap_or("");
        let fraction = parts.next().unwrap_or("");
        if integer.is_empty()
            || !integer.bytes().all(|b| b.is_ascii_digit())
            || fraction.len() > 2
            || !fraction.bytes().all(|b| b.is_ascii_digit())
            || parts.next().is_some()
        {
            return Err(Error::Input(
                "Tamanho_KB inválido. Use um número não negativo com até duas casas decimais."
                    .into(),
            ));
        }
        let integer = integer
            .parse::<u128>()
            .map_err(|_| Error::Input("Tamanho_KB muito grande.".into()))?;
        let fraction = format!("{fraction:0<2}").parse::<u128>().unwrap_or(0);
        let numerator = integer
            .checked_mul(100)
            .and_then(|v| v.checked_add(fraction))
            .and_then(|v| v.checked_mul(1024))
            .ok_or_else(|| Error::Input("Tamanho_KB muito grande.".into()))?;
        let max = numerator
            .checked_add(512)
            .ok_or_else(|| Error::Input("Tamanho_KB muito grande.".into()))?
            / 100;
        Ok(Self {
            min: u64::try_from(numerator.saturating_sub(512).div_ceil(100))
                .map_err(|_| Error::Input("Tamanho_KB muito grande.".into()))?,
            max: u64::try_from(max).map_err(|_| Error::Input("Tamanho_KB muito grande.".into()))?,
        })
    }

    pub fn contains(self, bytes: u64) -> bool {
        (self.min..=self.max).contains(&bytes)
    }
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: SizeRange,
    pub size_kib: String,
    pub created_local: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteMetadata {
    pub name: Option<String>,
    pub bytes: Option<u64>,
    /// Data HTTP/servidor: não equivale ao CreationTime local do schema.
    pub last_modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_locale_rounding_and_empty_files_without_floats() {
        assert_eq!(
            SizeRange::from_kib("13").unwrap(),
            SizeRange {
                min: 13307,
                max: 13317
            }
        );
        assert!(SizeRange::from_kib("1,12").unwrap().contains(1152));
        assert!(SizeRange::from_kib("1.234,56").unwrap().contains(1_264_189));
        assert!(SizeRange::from_kib("0").unwrap().contains(0));
        assert!(!SizeRange::from_kib("13").unwrap().contains(13000));
        for bad in ["NaN", "-1", "1,234", "1.2.3", "inf", ""] {
            assert!(SizeRange::from_kib(bad).is_err());
        }
    }
}
