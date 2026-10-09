#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferEngine {
    Boost,
    Tdlib,
}

impl TransferEngine {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("") | Some("tdlib") => Ok(Self::Tdlib),
            Some("boost") => Ok(Self::Boost),
            Some(other) => Err(format!("Unsupported transfer engine: {other}")),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Boost => "TeraRelay Boost",
            Self::Tdlib => "TDLib/C++",
        }
    }

    pub fn uses_tdlib(self) -> bool {
        matches!(self, Self::Tdlib)
    }
}

pub fn tdlib_platform_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

#[tauri::command]
pub fn cmd_tdlib_transfer_supported() -> bool {
    tdlib_platform_supported()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_transfers_keep_the_historic_tdlib_default() {
        assert_eq!(TransferEngine::parse(None).unwrap(), TransferEngine::Tdlib);
    }

    #[test]
    fn explicit_engines_parse_strictly() {
        assert_eq!(
            TransferEngine::parse(Some("boost")).unwrap(),
            TransferEngine::Boost
        );
        assert_eq!(
            TransferEngine::parse(Some("tdlib")).unwrap(),
            TransferEngine::Tdlib
        );
        assert!(TransferEngine::parse(Some("automatic")).is_err());
    }
}
