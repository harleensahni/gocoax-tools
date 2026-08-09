# Captured register fixtures — GoCoax MA2500 (192.0.2.250, MoCA 2.5, 2-node net)

Real `/ms/<cmd>` responses captured 2026-08-08. Each file is the raw
`{"data":[...]}` body. During implementation, copy these into
`crates/gocoax/tests/fixtures/` and assert decoders against the verified values
below.

| File | Command | Request body | Verified decode |
|---|---|---|---|
| `localInfo_0x15.json` | `/ms/0/0x15` | `{"data":[]}` | `[11]=0x25`→MoCA 2.5; `[12]=0x03`→nodes {0,1}; `[21,22]`→SOC "1.18.15"; `[0]=1`→myNodeId; `[5]=1`→link up |
| `netInfo_0x16.json` | `/ms/0/0x16` | `{"data":[0]}` | per-node info; `[4]&0xff=0x25`→node MoCA 2.5 |
| `macInfo_0x103.json` | `/ms/1/0x103/GET` | `{"data":[0]}` | `94:cc:04:00:00:01` |
| `frameInfo_0x14.json` | `/ms/0/0x14` | `{"data":[0]}` | txgood`[12,13]`=317682; txbad`[30,31]`=0; txdropped`[48,49]`=0; rxgood`[66,67]`; rxbad`[84,85]`=0; rxdropped`[102,103]`=0x2e=46 |
| `ethInfo_0x307.json` | `/ms/1/0x307/GET` | `{"data":[0]}` | link/speed/duplex words |
| `ipAddr_0x20b.json` | `/ms/1/0x20b/GET` | `{"data":[0]}` | `0xc00002fa`→192.0.2.250 |
| `lof_0x1003.json` | `/ms/0/0x1003/GET` | `{"data":[0]}` | `0x47e`=1150 (beacon channel) |
| `fmrInfo_0x1D_node0.json` | `/ms/0/0x1D` | `{"data":[1,2]}` (`1<<node`, finalVer=2) | OFDM params, MoCA 2.x payload starts at word 10. Traced golden: NPER self(0→0)=701 (matches UI screenshot exactly), NPER 0→1=3656 (matches UI screenshot exactly). VLPER self=701, 0→1=0. Captured with the CORRECT finalVer=2 — an earlier finalVer=37 capture returned zeros at word 10 and was wrong. |

## PHY-rate formula constants (from `device-pages/phyRates.html`)

```
MAX_NUM_NODES     = 16
LDPC_LEN_100MHZ   = 3900     FFT_LEN_100MHZ = 512
LDPC_LEN_50MHZ    = 1200     FFT_LEN_50MHZ  = 256

rate(NPER/VLPER, 100MHz) = floor(LDPC_LEN_100MHZ * ofdmb / ((FFT_LEN_100MHZ + (gap+10)*2) * 46))
rate(NPER/VLPER,  50MHz) = floor(LDPC_LEN_50MHZ  * ofdmb / ((FFT_LEN_50MHZ  + (gap*2+10)) * 26))
GCD(2.x self)            = floor(LDPC_LEN_100MHZ * ofdmbGcd / ((FFT_LEN_100MHZ + (gap+10)*2) * 46))
GCD(1.x self)            = floor(LDPC_LEN_50MHZ  * ofdmbGcd / ((FFT_LEN_50MHZ  + (gap*2+10)) * 26))
```

fmrInfo body: `data = 1<<node_id` (currNodeMask), `data2 = finalVer`
(`1` if min(ncMocaVer,nodeMocaVer) < 0x20 else `2`). FMR payload read starts at
`readIndx = 10`; per-node MoCA version selects the bit-unpack layout — see the
`refreshPage()` function in `phyRates.html`.
