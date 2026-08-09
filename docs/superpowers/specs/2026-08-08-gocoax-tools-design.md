# gocoax-tools — Design Spec

**Date:** 2026-08-08
**Status:** Approved design → ready for implementation planning
**Scope this cycle:** core library crate + Prometheus exporter

---

## 1. Goal

Lightweight Rust tooling to observe GoCoax MoCA adapters (MaxLinear MXL371x
family) via their web interface, exposing their stats as Prometheus metrics for
Grafana. This is the first cycle of a larger effort whose end goal is
**detect issues → reboot adapters to remediate**. Reboot support lands in the
core crate now; the *decision* to reboot is a later phase.

### Deliverables (this cycle)

- **`gocoax`** — core library crate: register protocol client, typed decoders,
  and `reboot()`.
- **`gocoax-exporter`** — Prometheus exporter binary: polls the configured
  adapters and serves `/metrics`.
- **`gocoax` CLI** — a thin binary in the core crate (`gocoax status <device>`,
  `gocoax reboot <device>`) to exercise/test the library by hand.
- **Discovery** — `gocoax discover` with three modes to find adapters on the
  LAN: (a) HTTP fingerprint scan (InterNiche server signature), (b) MAC/OUI
  filter (ARP table filtered by MoCA vendor OUIs), (c) MoCA self-report (one
  authenticated adapter enumerates all coax nodes via `netInfo`/`nodeBitMask`).

### Out of scope (future cycles)

- **Phase 3 — auto-remediation** (detect → reboot). Design deferred; see §12.
- **Phase 4 — firmware upgrade** (bulk flashing). Highest risk; deferred.
- **SNR / per-subcarrier metrics** — not exposed on the two pages we use.

---

## 2. Device protocol (verified against 192.0.2.250)

The web UI is a thin shell. Pages (`devStatus.html`, `phyRates.html`) load
`main.js` and call `Refresh()` on load; all data comes from a **register-read
protocol** over HTTP.

```
Server:  InterNiche Technologies WebServer 2.0   (plain HTTP, no TLS)
Auth:    HTTP Basic (admin:<password>)           — stateless, sent every request
CSRF:    any GET → Set-Cookie: csrf_token=<32 hex>; SameSite=Strict
Read:    POST /ms/<n>/<hexcmd>[/GET]
           headers: Authorization: Basic, X-CSRF-TOKEN: <token>, Cookie: csrf_token=<token>
           body:    {"data":[0]}
           → 200   {"data":["0x…","0x…", …]}     — array of u32 words (hex strings)
Reboot:  POST /ms/1/0xb00
```

### Session / CSRF behavior (empirically tested)

- **No login step.** Basic Auth is stateless; there is no session to create.
- **A csrf_token is reusable indefinitely** across calls. Each GET issues a
  *new* random token, but old tokens keep working (issuing new ones does not
  invalidate old ones).
- On the **read** endpoints the token is **not enforced** — POSTs with no token
  or a wrong token still return 200. Only Basic Auth actually gates reads.
  (The reboot/write endpoints were **not** tested for enforcement, since that
  means rebooting the device; we assume they *may* enforce it and send a valid
  token anyway.)

**Client consequence:** fetch **one** token lazily on first use, cache it for the
client's lifetime, attach it to every POST. On a `403` (only plausible from
writes), refetch once and retry. Net cost: one GET per client lifetime, then N
POSTs — no per-call handshake.

### Command map

Device Status page:

| Command | Form name | Data |
|---|---|---|
| `/ms/0/0x15` | localInfo | SOC version, node id, MoCA versions, node bitmask |
| `/ms/0/0x16` | netInfo | per-node network info (26 words) |
| `/ms/1/0x103/GET` | macInfo | MAC address (2 words) |
| `/ms/0/0x14` | frameInfo | Ethernet Tx/Rx good/bad/dropped counters |
| `/ms/1/0x307/GET` | ethInfo | Ethernet link status/speed/duplex |
| `/ms/1/0x20b/GET` | ipAddr | IP address (1 word) |
| `/ms/0/0x1003/GET` | lof | channel frequencies |
| `/ms/1/0x303/GET` | ChipID | chip id |
| `/ms/1/0xb17` | gpio | gpio state |

PHY Rates page:

| Command | Form name | Data |
|---|---|---|
| `/ms/0/0x15` | localInfo | myNodeID, mocaNetVer, nodeBitMask |
| `/ms/0/0x16` | netInfo | per-node info incl. MoCA version (`[4] & 0xff`: `0x25`=2.5, `0x20`=2.0) |
| `/ms/0/0x1D` | fmrInfo | per-node FMR / OFDM bit-loading params (multi-data) |

Reboot: `/ms/1/0xb00`.

### Decoding notes (ported from page JS)

Values arrive as u32 words; the page JS decodes with bit math. Examples verified:

- **IP** (`0x20b`): `0xc00002fa` → `192.0.2.250` (byte-per-octet).
- **MAC** (`0x103`): `0x94cc0400 0x00010000` → `94:cc:04:00:00:01`.
- **Eth counters** (`0x14`): 64-bit counts assembled from word pairs, e.g.
  `txgood = frame[12]<<32 | frame[13]`, `txbad = [30]|[31]`, `txdropped = [48]|[49]`,
  `rxgood = [66]|[67]`, `rxbad = [84]|[85]`, `rxdropped = [102]|[103]`.
- **MoCA version** (`0x16[4] & 0xff`): `0x25`=2.5, `0x20`=2.0.

**PHY rate** is **computed, not returned.** From `fmrInfo` OFDM params
(`gap`, `ofdmb`) per node, branching on MoCA 2.0/2.5 and NPER/VLPER/GCD:

```
rate ≈ floor( LDPC_LEN * ofdmb / ((FFT_LEN + gap_terms) * N) )
   50MHz:  LDPC_LEN_50MHZ,  FFT_LEN_50MHZ,  N=26
  100MHz:  LDPC_LEN_100MHZ, FFT_LEN_100MHZ, N=46
```

The exact constants and index offsets are in the device's `phyRates.html` JS
(captured locally). These formulas port directly to Rust and are validated
against known-good outputs (see §10).

---

## 3. Workspace layout

Cargo workspace at repo root:

```
gocoax-tools/
  Cargo.toml                 # [workspace]
  crates/
    gocoax/                  # core library (+ CLI bin)
      src/
        lib.rs
        client.rs            # Client: auth, csrf cache, read(), reboot()
        ms.rs                # command constants; request build + response parse
        decode.rs            # typed decoders (DeviceStatus, EthCounters, …)
        phy.rs               # NPER/VLPER/GCD rate formulas
        config.rs            # global creds + [[device]] overrides
        error.rs             # error enum (thiserror)
        bin/gocoax.rs         # thin CLI: status / reboot
    gocoax-exporter/         # Prometheus exporter binary
      src/
        main.rs              # config load, axum server
        scrape.rs            # per-device fan-out + global deadline
        metrics.rs           # decoded structs → Prometheus text
  docs/superpowers/specs/…   # this spec
```

---

## 4. HTTP stack (decided: async tokio)

- **Client:** `reqwest` (async), `default-features = false` + `["json"]` —
  HTTP-only, no TLS (device is plain HTTP).
- **Server:** `axum` for `/metrics`.
- **Runtime:** `tokio` (full).
- **Support:** `serde`/`serde_json` (responses + config), `toml` (config),
  `thiserror` (errors), a small args parser (`clap`) for the CLI/exporter flags.

Rationale (recorded from brainstorming): binary-size difference vs a blocking
stack is negligible on x86_64 (measured 1.2 MB vs 2.0 MB, HTTP-only). Async is
chosen for **hard global-deadline cancellation** on scrapes and a smoother path
to a future Option-B self-deciding remediation daemon. Compile time is a
non-concern for this project.

All HTTP is confined to `client.rs` behind a small async interface
(`read(cmd) -> Vec<u32>`, `reboot()`), so the stack choice is not a lock-in.

---

## 5. Configuration (TOML)

Global credentials with optional per-device overrides.

```toml
username = "admin"          # global default
password = "…"              # inline, OR password_env = "VAR", OR password_file = "path"
listen   = "0.0.0.0:9420"
request_timeout = "8s"      # per-request
connect_timeout = "3s"
scrape_deadline = "9s"      # global cap per /metrics scrape

[[device]]
name = "moca-1"
host = "192.0.2.250"     # inherits global creds

[[device]]
name = "living-room"
host = "192.0.2.251"
username = "admin"          # override
password_env = "LR_PW"      # override; may reference env instead of inline
```

- Credential resolution per level: `password` (inline) | `password_env` |
  `password_file`, device overriding global.
- `config.toml` is git-ignored by default (may contain secrets).

---

## 6. Exporter behavior

- **Scrape-on-demand.** Each `GET /metrics` triggers a fresh poll of all
  devices; Prometheus's `scrape_interval` drives cadence. No background loop, no
  stale cache.
- **Concurrency.** One `tokio` task per device; all run concurrently.
- **Global deadline.** The whole fan-out is wrapped in
  `tokio::time::timeout(scrape_deadline, …)`. On expiry, in-flight requests are
  dropped (true cancellation); devices that finished report real data, the rest
  report `gocoax_up=0`.
- **Per-device isolation.** A device that is unreachable / auth-failing /
  timing out sets `gocoax_up{device}=0`, increments
  `gocoax_scrape_errors_total{device,reason}` with a classified `reason`, and
  never fails the whole scrape. Errors logged at `warn`.
- **`/metrics` always returns 200** even when every device is down — a dead
  device is data (`up=0`), not an exporter error. The exporter only fails the
  endpoint if the exporter itself is broken (→ Prometheus `up{job}=0`).
- **Error classification.** `Error` → `reason` label mapping is a single
  function: `Http(connect)→unreachable`, `Timeout→timeout`, `Auth→auth`,
  `Csrf→csrf`, `Http(status)→http_status`, `Decode→decode`.
- **CSRF.** Token cached per `Client`; refetched once on `403` then retried.

---

## 7. Metrics catalog

Common labels: `device` (config name); `host`, `mac`, `ip` on the info metric.

```
# Health / observability — exporter-synthesized (works even when the device
# is unreachable and returns nothing). Note Prometheus also auto-generates
# up{job="gocoax"} for the exporter process itself (layer 1). These are layer 2:
gocoax_up{device}                                    1/0 current device read health
gocoax_scrape_errors_total{device,reason}            counter; reason=unreachable|timeout|auth|csrf|http_status|decode
gocoax_last_success_timestamp_seconds{device}        unix ts of last clean read
gocoax_scrape_duration_seconds{device}               per-device scrape time

gocoax_info{device,host,mac,ip,soc_version,moca_version}   =1 (label carrier)

gocoax_moca_link_up{device}                          1/0
gocoax_moca_nodes{device}                            node count
gocoax_node_moca_version{device,node}                20 | 25

# PHY rate matrix — core link-quality signal
gocoax_phy_rate_mbps{device,from_node,to_node,type}  type = nper|vlper
gocoax_phy_rate_gcd_mbps{device,node}

# Ethernet
gocoax_ethernet_link_up{device,port}
gocoax_ethernet_speed_mbps{device,port}
gocoax_ethernet_tx_frames_total{device,port,status}  status = good|bad|dropped  (counter)
gocoax_ethernet_rx_frames_total{device,port,status}  (counter)
```

Counters use `_total` and are monotonic so Grafana `rate()` shows error-rate
spikes. This set is exactly what phase-3 remediation will threshold on (low PHY
rate, link down, rising bad/dropped).

---

## 8. Core library API (sketch)

```rust
pub struct Client { /* base_url, creds, http: reqwest::Client, csrf: RwLock<Option<String>> */ }

impl Client {
    pub fn new(host: &str, creds: Credentials, opts: ClientOpts) -> Self;

    /// Low-level register read: POST /ms/<cmd>, parse {"data":[…]} → words.
    pub async fn read(&self, cmd: MsCmd) -> Result<Vec<u32>, Error>;

    /// High-level typed reads (compose several `read`s + decode).
    pub async fn device_status(&self) -> Result<DeviceStatus, Error>;
    pub async fn phy_rates(&self)     -> Result<PhyRates, Error>;

    /// Control.
    pub async fn reboot(&self) -> Result<(), Error>;
}
```

- Decoders live in `decode.rs`/`phy.rs` as pure functions
  `decode(&[u32]) -> Result<T, Error>`, independently unit-testable.
- `reboot()` is in the crate but the **exporter never calls it** (sets up
  phase 3; usable now only via the CLI).

---

## 9. Error handling

- **Length-checked decoders.** Never index past a short `data` array — return a
  decode error, never panic.
- **Per-device isolation** in the exporter (see §6): device errors → `up=0`.
- **Timeouts:** separate connect (`connect_timeout`) and total
  (`request_timeout`); global `scrape_deadline` over the whole fan-out.
- **CSRF 403:** refetch token once, retry once.
- **Auth 401:** `up=0` with a clear logged reason.
- Error enum via `thiserror`: `Http`, `Auth`, `Csrf`, `Decode { cmd, reason }`,
  `Timeout`, `Config`.

---

## 10. Testing strategy

- **Decoder unit tests from real fixtures.** Capture a full `/ms/` response set
  from the live device into `crates/gocoax/tests/fixtures/`; assert decoders
  produce known values (IP `192.0.2.250`, MAC `94:cc:04:00:00:01`, etc.).
- **PHY-rate golden tests.** Fixture `fmrInfo` + `netInfo` → assert the rates
  seen in the UI (**701 / 3656 Mbps**), proving the ported formulas correct.
- **Protocol tests.** Mock HTTP server (e.g. `wiremock`/`httpmock`) for the
  csrf-cache, 403-refetch-retry, and timeout paths.
- **Exporter test.** Feed a faked client → assert `/metrics` text output
  (metric names, labels, `up` handling on device failure).
- **Live smoke test.** `#[ignore]` integration test hitting the real device,
  run manually.

**Implementation step 0:** capture the fixture set from 192.0.2.250 (all
Device Status + PHY Rates commands) before writing decoders, so decoders are
built and tested against ground truth.

---

## 11. Security / operational notes

- Device speaks plain HTTP with weak/absent CSRF enforcement on reads; the only
  real gate is Basic Auth. Keep credentials in git-ignored config / env / files.
- Exporter is read-only. `reboot()` is reachable only via the CLI this cycle.
- Bind the exporter to a trusted interface; it exposes device presence/stats.

---

## 12. Future phases (context, not built now)

- **Phase 3 — remediation.** Who triggers the reboot is undecided. Options:
  - **A (recommended): Alertmanager webhook → tiny stateless remediator** that
    calls `gocoax::reboot()`. Reuses Prometheus alert rules for
    hysteresis/dedup/cooldown/silencing and gives an audit trail. Event-driven.
  - **B: self-deciding daemon** — one long-running async process that polls,
    evaluates thresholds in memory (cooldowns, consecutive-bad counts), and
    reboots. The async stack chosen here suits this path.
  - **C: manual** — reboot via CLI after seeing Grafana.
- **Phase 4 — firmware upgrade.** `upgrade.html` file-upload flashing; highest
  risk; its own careful spec.

---

## 13. Assumptions to confirm during implementation

- Exact PHY-rate constants/indices (`LDPC_LEN_*`, `FFT_LEN_*`, `fmrInfo` offsets)
  transcribed from device JS and validated against golden 701/3656 values.
- Whether reboot/write endpoints enforce the csrf token (send it regardless).
- Ethernet counter word indices confirmed against fixtures.
- Multi-node `netInfo`/`fmrInfo` iteration over `nodeBitMask` matches device.
