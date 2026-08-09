use crate::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy)]
pub struct MsCmd {
    pub space: u8,
    pub code: &'static str,
    pub get_suffix: bool,
}

impl MsCmd {
    pub const fn new(space: u8, code: &'static str, get_suffix: bool) -> Self {
        Self { space, code, get_suffix }
    }
    pub fn path(&self) -> String {
        if self.get_suffix {
            format!("/ms/{}/{}/GET", self.space, self.code)
        } else {
            format!("/ms/{}/{}", self.space, self.code)
        }
    }
}

pub const LOCAL_INFO: MsCmd = MsCmd::new(0, "0x15", false);
pub const NET_INFO: MsCmd = MsCmd::new(0, "0x16", false);
pub const MAC_INFO: MsCmd = MsCmd::new(1, "0x103", true);
pub const FRAME_INFO: MsCmd = MsCmd::new(0, "0x14", false);
pub const ETH_INFO: MsCmd = MsCmd::new(1, "0x307", true);
pub const IP_ADDR: MsCmd = MsCmd::new(1, "0x20b", true);
pub const LOF: MsCmd = MsCmd::new(0, "0x1003", true);
pub const FMR_INFO: MsCmd = MsCmd::new(0, "0x1D", false);
pub const REBOOT: MsCmd = MsCmd::new(1, "0xb00", false);

#[derive(Deserialize)]
struct RawMs {
    data: Vec<String>,
}

pub fn parse_ms_response(body: &str) -> Result<Vec<u32>> {
    let raw: RawMs = serde_json::from_str(body)
        .map_err(|e| Error::Decode { cmd: "ms".into(), reason: format!("json: {e}") })?;
    raw.data
        .iter()
        .map(|w| {
            let s = w.trim().trim_start_matches("0x");
            u32::from_str_radix(s, 16)
                .map_err(|e| Error::Decode { cmd: "ms".into(), reason: format!("word {w:?}: {e}") })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_formats_get_suffix() {
        assert_eq!(MAC_INFO.path(), "/ms/1/0x103/GET");
        assert_eq!(NET_INFO.path(), "/ms/0/0x16");
        assert_eq!(REBOOT.path(), "/ms/1/0xb00");
    }

    #[test]
    fn parses_word_array() {
        let v = parse_ms_response(r#"{"data":["0xc00002fa"]}"#).unwrap();
        assert_eq!(v, vec![0xc00002fa]);
    }

    #[test]
    fn parses_multi_word() {
        let v = parse_ms_response(r#"{"data":["0x94cc0400","0x00010000"]}"#).unwrap();
        assert_eq!(v, vec![0x94cc0400, 0x00010000]);
    }

    #[test]
    fn rejects_bad_word() {
        assert!(parse_ms_response(r#"{"data":["0xZZ"]}"#).is_err());
    }
}
