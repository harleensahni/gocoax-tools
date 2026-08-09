//! PHY-rate decoder.
//!
//! Ports the device's own JavaScript FMR-unpacking + rate formulas
//! (`device-pages/phyRates.html`, `refreshPage()`) to Rust. See that file
//! for the authoritative algorithm; this module follows it line-for-line,
//! including its quirks, so that decoded rates match what the device's own
//! UI displays.

use crate::decode::{get, MAX_NODES};
use crate::Result;

pub const LDPC_LEN_100MHZ: u32 = 3900;
pub const LDPC_LEN_50MHZ: u32 = 1200;
pub const FFT_LEN_100MHZ: u32 = 512;
pub const FFT_LEN_50MHZ: u32 = 256;

/// NPER/VLPER/GCD rate for 2.x (100 MHz) nodes.
pub fn rate_100mhz(ofdmb: u32, gap: u32) -> u32 {
    (LDPC_LEN_100MHZ * ofdmb) / ((FFT_LEN_100MHZ + (gap + 10) * 2) * 46)
}

/// NPER/VLPER/GCD rate for 1.x (50 MHz) nodes.
pub fn rate_50mhz(ofdmb: u32, gap: u32) -> u32 {
    (LDPC_LEN_50MHZ * ofdmb) / ((FFT_LEN_50MHZ + (gap * 2 + 10)) * 26)
}

/// One entry pf the PHY-rate matrix: the rate from `from_node` to `to_node`
/// (a self-link when `from_node == to_node`, reported by the device as the
/// per-node GCD rate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhyLink {
    pub from_node: u32,
    pub to_node: u32,
    pub nper_mbps: u32,
    pub vlper_mbps: u32,
}

/// The full PHY-rate matrix across all nodes in the network, assembled (by
/// a later task) from one [`decode_fmr`] call per present node.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhyRates {
    pub gcd_mbps: Vec<(u32 /* node */, u32 /* mbps */)>,
    pub links: Vec<PhyLink>,
    pub node_versions: Vec<(u32 /* node */, u8 /* raw MoCA version byte, e.g. 0x25 */)>,
}

const CMD: &str = "0x1D";

/// Unpack `(gapNper, gapVLper, ofdmbNper, ofdmbVLper)` from a MoCA 2.x FMR
/// payload word pair per `refreshPage()`'s `(fmrPayloadVer == 0x20 || 0x25)`
/// branch, and return the advanced `read_idx`.
fn unpack_2x(fmr: &[u32], read_idx: usize, alignment: bool) -> Result<(u32, u32, u32, u32, usize)> {
    if alignment {
        let w0 = get(fmr, read_idx, CMD)?;
        let w1 = get(fmr, read_idx + 1, CMD)?;
        let gap_nper = (w0 >> 24) & 0xff;
        let gap_vlper = (w0 >> 16) & 0xff;
        let ofdmb_nper = w0 & 0xffff;
        let ofdmb_vlper = (w1 >> 16) & 0xffff;
        Ok((gap_nper, gap_vlper, ofdmb_nper, ofdmb_vlper, read_idx + 1))
    } else {
        let w0 = get(fmr, read_idx, CMD)?;
        let w1 = get(fmr, read_idx + 1, CMD)?;
        let gap_nper = (w0 >> 8) & 0xff;
        let gap_vlper = w0 & 0xff;
        let ofdmb_nper = (w1 >> 16) & 0xffff;
        let ofdmb_vlper = w1 & 0xffff;
        Ok((gap_nper, gap_vlper, ofdmb_nper, ofdmb_vlper, read_idx + 2))
    }
}

/// Unpack `(gapNper, gapVLper, ofdmbNper, ofdmbVLper)` from a MoCA 1.x FMR
/// payload word per `refreshPage()`'s `else` (1.x) branch, and return the
/// advanced `read_idx`.
///
/// NOTE: this mirrors a quirk in the device JS: `gapVLper`/`ofdmbVLper` are
/// computed into local `tempGapVlper`/`ofdmbVLper` but `gapVLper` itself is
/// never reassigned from its initial `0`, so VLPER is always reported as 0
/// for 1.x peers. Preserved here for fidelity with the device UI.
fn unpack_1x(fmr: &[u32], read_idx: usize, alignment: bool) -> Result<(u32, u32, u32, u32, usize)> {
    let w0 = get(fmr, read_idx, CMD)?;
    if alignment {
        let gap_nper = (w0 & 0xf800_0000) >> 27;
        let ofdmb_nper = (w0 & 0x07ff_0000) >> 16;
        Ok((gap_nper, 0, ofdmb_nper, 0, read_idx))
    } else {
        let gap_nper = (w0 & 0x0000_f800) >> 11;
        let ofdmb_nper = w0 & 0x0000_07ff;
        Ok((gap_nper, 0, ofdmb_nper, 0, read_idx + 1))
    }
}

/// Advance `read_idx` past an absent node's slot in the FMR payload, per
/// `refreshPage()`'s `if (!(nodeBitMask & (1 << jd)))` branch. Not exercised
/// by the committed (contiguous, 2-node) fixture; validated only by
/// inspection of the JS pending a hardware capture with gaps in the node
/// bitmask.
fn skip_absent(read_idx: usize, alignment: bool, entry_node_moca_ver: u8) -> usize {
    if entry_node_moca_ver >= 0x20 {
        if alignment {
            read_idx + 1
        } else {
            read_idx + 2
        }
    } else if alignment {
        read_idx
    } else {
        read_idx + 1
    }
}

/// Decode one node's `0x1D` (FMR info) response into a `PhyLink` per present
/// peer, following `refreshPage()` in `phyRates.html` exactly.
///
/// - `from_node`: the node whose FMR payload `fmr` is (the "id" in the JS).
/// - `node_bitmask`: bit `i` set means node `i` is present on the network.
/// - `node_vers[i]`: MoCA version byte of node `i` (e.g. `0x25`), indexed by
///   node id; `0` if unknown/absent.
/// - `nc_moca_ver`: the network coordinator's MoCA version byte.
///
/// Returns one `PhyLink` per present peer `jd` in `0..MAX_NODES` (including
/// the self link `from_node == jd`, which the device UI shows as the
/// per-node GCD rate).
pub fn decode_fmr(
    from_node: u32,
    node_bitmask: u32,
    node_vers: &[u8],
    nc_moca_ver: u8,
    fmr: &[u32],
) -> Result<Vec<PhyLink>> {
    // mocaNodeVer = netInfo[nodeId[numNode]][4] & 0xFF -- the *entry* (from)
    // node's own MoCA version, fixed for the whole inner jd loop.
    let entry_node_moca_ver = node_vers.get(from_node as usize).copied().unwrap_or(0);
    // entryNodePayloadVer = min(mocaNodeVer, ncMocaVer); only used by the
    // sub-2.x-NC branch below, but computed once up front like the JS does.
    let entry_node_payload_ver = entry_node_moca_ver.min(nc_moca_ver);

    let mut read_idx: usize = 10;
    let mut alignment = true;
    let mut links = Vec::new();

    for jd in 0..MAX_NODES {
        let present = node_bitmask & (1 << jd) != 0;
        if !present {
            read_idx = skip_absent(read_idx, alignment, entry_node_moca_ver);
            alignment = !alignment;
            continue;
        }

        // fmrPayloadVer: min(entryNodePayloadVer, jd's mocaVer) for a
        // sub-2.x NC, else just the entry node's own MoCA version.
        let fmr_payload_ver = if nc_moca_ver < 0x20 {
            let jd_ver = node_vers.get(jd as usize).copied().unwrap_or(0);
            entry_node_payload_ver.min(jd_ver)
        } else {
            entry_node_moca_ver
        };

        let (gap_nper, gap_vlper, ofdmb_nper, ofdmb_vlper, new_read_idx) =
            if fmr_payload_ver == 0x20 || fmr_payload_ver == 0x25 {
                unpack_2x(fmr, read_idx, alignment)?
            } else {
                unpack_1x(fmr, read_idx, alignment)?
            };
        read_idx = new_read_idx;
        alignment = !alignment;

        // Rate calculation, ported verbatim from refreshPage()'s common
        // post-processing (it runs the same way regardless of which unpack
        // branch above ran):
        //   VLPER: 0 if gapVLper == 0, else the 100 MHz formula.
        //   NPER: 0 if gapNper == 0; the 50 MHz formula only in the narrow
        //         edge case of an exact MoCA-2.0 (not 2.5) payload with no
        //         VLPER gap; the 100 MHz formula otherwise (this covers
        //         genuine 1.x peers too -- a quirk of the device firmware,
        //         preserved here for fidelity with the device UI).
        let vlper_mbps = if gap_vlper == 0 { 0 } else { rate_100mhz(ofdmb_vlper, gap_vlper) };

        let nper_mbps = if gap_nper == 0 {
            0
        } else if gap_vlper == 0 && fmr_payload_ver == 0x20 {
            rate_50mhz(ofdmb_nper, gap_nper)
        } else {
            rate_100mhz(ofdmb_nper, gap_nper)
        };

        links.push(PhyLink { from_node, to_node: jd, nper_mbps, vlper_mbps });
    }

    Ok(links)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Deterministic formula checks (independent of the fixture unpack).
    // rate_100mhz(1000,10) = 3900*1000 / ((512 + (10+10)*2)*46)
    //                      = 3_900_000 / (552*46=25392) = 153
    // rate_50mhz(1000,10)  = 1200*1000 / ((256 + (10*2+10))*26)
    //                      = 1_200_000 / (286*26=7436) = 161
    #[test]
    fn rate_formulas_are_exact() {
        assert_eq!(rate_100mhz(1000, 10), 153);
        assert_eq!(rate_50mhz(1000, 10), 161);
    }
}
