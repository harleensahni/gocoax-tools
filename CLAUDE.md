# gocoax-tools — project memory

Rust workspace for observing GoCoax MoCA adapters (MaxLinear MXL371x family)
over their web management interface, and **automatically rebooting stuck ones**.
Three binaries: the `gocoax` CLI (status/reboot/discover), the
`gocoax-exporter` Prometheus exporter, and `gocoax-remediator` (phase 3
auto-reboot — polls Prometheus, reboots on sustained problems with a cooldown +
circuit breaker, `dry_run`-first; see `docs/remediator.md`). All three ship in
one container image.

## Workspace map

```
gocoax-tools/
  Cargo.toml                       # [workspace], shared deps (tokio, reqwest, axum, ...)
  config.example.toml              # template for config.toml (git-ignored)
  grafana-dashboard.json           # importable Grafana dashboard (21 panels across 8 rows)
  crates/
    gocoax/                        # core library + CLI bin
      src/
        lib.rs                    # module list + public re-exports
        error.rs                  # Error enum (thiserror): Http/Timeout/Auth/Csrf/HttpStatus/Decode/Config
        ms.rs                     # MsCmd command map + parse_ms_response (JSON -> Vec<u32>)
        decode.rs                 # bounds-checked primitives + DeviceStatus/EthCounters/EthPort/MocaNode
        phy.rs                    # FMR unpack + PHY rate formulas (NPER/VLPER/GCD), PhyRates/PhyLink
        config.rs                 # Config/Device/ResolvedCreds: global + per-device creds, password sources
        client.rs                 # Client: HTTP Basic auth, csrf cache, read(), device_status(), phy_rates(), moca_nodes(), reboot()
        discover.rs                # LAN discovery: HTTP fingerprint scan, MAC/OUI filter, MoCA self-report
        bin/gocoax.rs              # CLI: `gocoax status|reboot|discover`
      tests/
        decode_fixtures.rs         # decoder goldens against real captured /ms/ responses
        client_mock.rs             # wiremock-based csrf/retry/timeout protocol tests
        discover_http.rs           # discovery unit/integration tests
        fixtures/*.json            # captured raw /ms/ response bodies (ground truth)
    gocoax-exporter/                # Prometheus exporter binary
      src/
        main.rs                    # config load + axum server, GET /metrics
        scrape.rs                  # AppState, per-device fan-out, global deadline, error/last-success counters, reason_for()
        metrics.rs                 # DeviceOutcome -> Prometheus text (render(), pure, no I/O)
        lib.rs                     # pub mod metrics; pub mod scrape;
      tests/
        metrics_render.rs           # render() text-format tests
        scrape_integration.rs       # scrape() against a mock device server incl. deadline expiry
    gocoax-remediator/              # phase-3 auto-reboot daemon
      src/
        config.rs                  # [remediator] table: prometheus_url, cooldown, breaker, rules, dry_run(default true), verbose
        prom.rs                    # Prometheus instant-query client -> device labels
        state.rs                   # cooldown + circuit-breaker state machine (pure, decide()/record_reboot())
        poller.rs                  # poll_once(): query rules -> reboot (injectable Rebooter), safety gates; AppState::new zero-inits reboot counters per device×rule (first-event visibility — keep)
        metrics.rs                 # remediator's own /metrics (reboots_total, circuit_open, ...)
        main.rs                    # config load + poll loop + axum /metrics
```

## The device protocol (the crux)

The device runs an **InterNiche embedded webserver**, plain HTTP (no TLS).
There is no login/session — every request (including reads) carries **HTTP
Basic auth**. All state (device status, PHY rates, node list) is read via a
**register-read protocol**, not REST-ish JSON endpoints:

```
Read:    POST /ms/<space>/<hexcmd>[/GET]
           headers: Authorization: Basic <user:pass>, X-CSRF-TOKEN: <token>,
                    Cookie: csrf_token=<token>
           body:    {"data":[...]}          (register args, often empty/[0])
           → 200    {"data":["0x..","0x..",...]}   (array of u32 words, hex strings)
Reboot:  POST /ms/1/0xb00   (FIRE-AND-FORGET — see below)
```

**Reboot is fire-and-forget.** The adapter power-cycles the instant it receives
`0xb00` and drops the connection **without sending an HTTP response** (its own
web UI fires the POST with empty callbacks and reloads after 10s). So
`Client::reboot()` treats a **timeout / dropped connection after the request was
sent as SUCCESS** — only a connect failure (never reached the device) or a 401
is an error. Do NOT route reboot through the normal `read()` path (which waits
to parse a JSON body that never arrives → false "timeout" error). Verified by a
regression test in `tests/client_mock.rs` (`reboot_ok_when_device_drops_after_send`).

**CSRF token**: any GET (`/index.html`) returns `Set-Cookie: csrf_token=<hex>`.
Empirically the token is **reusable indefinitely** and reads **don't even
enforce it** (only Basic auth actually gates reads) — see `client.rs` doc
comment. So the client fetches the token **once**, lazily, caches it for the
`Client`'s lifetime, and sends it on every POST as both `X-CSRF-TOKEN` and
`Cookie`. On a `403`, the cached token is discarded, refetched once, and the
POST retried once (`Client::read` in `crates/gocoax/src/client.rs`).

**⚠️ CASE-SENSITIVE HEADERS (critical interop gotcha):** the InterNiche server
does **case-sensitive** HTTP header matching and only accepts `Authorization`
with a capital `A`. hyper/reqwest lowercase HTTP/1.1 header names by default,
so the device rejects every request with **401** unless the reqwest client is
built with **`.http1_title_case_headers()`** (see `Client::new`). This is NOT
catchable by the wiremock unit tests (wiremock is RFC-compliant /
case-insensitive) — it only surfaces against real hardware. The regression
test `crates/gocoax/tests/title_case_headers.rs` captures the client's raw
bytes and asserts the capitalized header, and was verified live
(exporter `/metrics` matches the device UI exactly: PHY 701/3656, MAC, IP,
rx_dropped 46, etc.). If you ever swap the HTTP client, preserve title-case
headers or auth breaks completely.

### Command map (`crates/gocoax/src/ms.rs`)

| Constant | Path | Purpose |
|---|---|---|
| `LOCAL_INFO` | `/ms/0/0x15` | node id, MoCA version, node bitmask, link state |
| `NET_INFO` | `/ms/0/0x16` | per-node network info (MoCA version, MAC) |
| `MAC_INFO` | `/ms/1/0x103/GET` | device MAC address |
| `FRAME_INFO` | `/ms/0/0x14` | Ethernet tx/rx frame counters |
| `ETH_INFO` | `/ms/1/0x307/GET` | per-port Ethernet link/speed/duplex |
| `IP_ADDR` | `/ms/1/0x20b/GET` | device IP address |
| `LOF` | `/ms/0/0x1003/GET` | beacon channel frequency |
| `FMR_INFO` | `/ms/0/0x1D` | per-node OFDM bit-loading params (PHY rate source) |
| `REBOOT` | `/ms/1/0xb00` | trigger device reboot |

## Decode gotchas (hard-won — see fixtures + ledger for how these were found)

- **PHY rate is COMPUTED, not returned.** `fmrInfo` (`0x1D`) gives OFDM
  `gap`/`ofdmb` params; the rate is
  `floor(LDPC_LEN * ofdmb / ((FFT_LEN + gap_terms) * N))`, with separate
  100 MHz (MoCA 2.x, `N=46`) and 50 MHz (MoCA 1.x, `N=26`) constant sets. See
  `phy.rs::rate_100mhz`/`rate_50mhz`, ported line-for-line from the device's
  `phyRates.html` JS (`refreshPage()`).
- **`fmrInfo` MUST be requested with `finalVer=2`** (for MoCA 2.x networks) —
  the body is `{"data":[<node bit>, <finalVer>]}`. Requesting with the wrong
  `finalVer` (e.g. a stale/wrong value like 37) silently returns a
  **zeroed payload** starting around word 10, not an error. This bit the
  first fixture capture (see `progress.md` Task 5 PREP) — always re-verify
  against a live device if FMR values look suspiciously blank.
- **Eth counters are 64-bit word pairs at fixed indices** into `frameInfo`
  (`0x14`): `tx_good=[12,13]`, `tx_bad=[30,31]`, `tx_dropped=[48,49]`,
  `rx_good=[66,67]`, `rx_bad=[84,85]`, `rx_dropped=[102,103]` — see
  `EthCounters::decode`.
- **`ethInfo` (`0x307`) per-port triples start at port index 1**, not 0 — the
  device UI itself skips port 0 on MXL371x. See `decode_eth_ports`.
- **`ethInfo` (`0x307`) is read BEST-EFFORT** in `Client::device_status`: the
  per-port link/speed feature was *added* in newer firmware (e.g. `1.18.15`);
  older adapters don't implement `0x307` and return **400**. A failed `0x307`
  read must NOT fail the whole status read — it falls back to empty `eth_ports`
  so a firmware-drifted device still reports `gocoax_up=1` with all its other
  data (the eth link/speed metrics are simply omitted; `metrics::render` already
  skips absent `eth_ports`). Propagating it would mark a healthy adapter down and
  make the remediator try to reboot it over a missing feature. Regression tests:
  `device_status_ok_when_eth_info_returns_400` + `..._populates_eth_ports_when_0x307_ok`
  in `tests/client_mock.rs`.
- **Node MoCA version** is `netInfo[4] & 0xff`, e.g. `0x25` → "2.5",
  `0x20` → "2.0" (used both in `DeviceStatus::decode`'s own moca_version via
  `localInfo[11]` and per-node in `decode_net_nodes`/`phy_rates`).
- **1.x FMR quirk preserved for fidelity**: for MoCA-1.x peers, `gapVLper` is
  always reported as 0 in the device's own JS (a local variable is computed
  but never assigned back) — `phy.rs::unpack_1x` mirrors this deliberately.
- **Verified goldens** (from `crates/gocoax/tests/fixtures/`, a real 2-node
  192.0.2.250 capture — exact UI match): IP `192.0.2.250`, MAC
  `94:cc:04:00:00:01`, SOC version `1.18.15`, PHY self-rate (node 0→0)
  `701 Mbps`, PHY node 0→1 `3656 Mbps`.

## Key decisions + why

- **Async stack: `tokio` + `reqwest` (no `cookies` feature) + `axum`.** Chosen
  over a blocking stack for (a) **hard global-deadline cancellation** on
  exporter scrapes (`tokio::time::timeout` truly drops in-flight requests) and
  (b) a smoother path to a future self-deciding remediation daemon (phase 3
  option B). The binary-size delta was measured negligible (~1.2 MB blocking
  vs ~2.0 MB async, HTTP-only) — size was not a real constraint either way.
- **HTTP-only, no TLS.** The device itself only speaks plain HTTP; `reqwest`
  is built with `default-features = false, features = ["json", "hickory-dns"]`.
- **`hickory-dns` resolver (not the default libc one).** The release container
  is a static **musl** binary on `scratch`, and musl's resolver does not expand
  `search`-domain / short hostnames via Docker's embedded DNS (`127.0.0.11`) —
  so `host = "first-floor-tv-moca"` in a compose stack would time out while a
  Go exporter (blackbox) on the same network resolves it fine. `hickory-dns` is
  a pure-Rust resolver that reads `resolv.conf` (search/ndots) itself, so short
  names resolve in the image. FQDNs and IPs work with any resolver.
- **Fetch-once-reuse CSRF**, not a token-per-request dance — reads don't even
  enforce the token; only Basic auth gates them (see protocol section above).
  This was verified empirically against the real device, not assumed.
- **Scrape-on-demand exporter**, no background polling loop: each
  `GET /metrics` triggers a fresh concurrent fan-out (one task per device),
  bounded by `scrape_deadline_secs`. Per-device isolation means one
  unreachable device never fails the whole scrape or the endpoint.
  `/metrics` **always returns 200**, even if every device is down — a dead
  device is data (`gocoax_up=0`), not an exporter failure.
- **Two-layer `up`**: Prometheus auto-generates `up{job="gocoax"}`
  for the exporter *process* (layer 1: is the exporter itself alive). This
  workspace additionally emits `gocoax_up{device}` (layer 2: is *this device*
  currently readable). Don't confuse the two when writing alerts.
- **Metric naming**: `gocoax_*` prefix; counters end in `_total`
  (`gocoax_scrape_errors_total`, `gocoax_ethernet_{tx,rx}_frames_total`) and
  are monotonic so Grafana `rate()` works.
- **The exporter is read-only** and never reboots — only `gocoax-remediator`
  (and the `gocoax reboot` CLI) call `Client::reboot()`. The remediator gates
  every reboot behind a cooldown + daily circuit breaker and defaults to
  `dry_run = true`.

## Conventions

- **Every register-array access goes through the bounds-checked
  `decode::get(words, idx, cmd)`** helper — it returns `Error::Decode` rather
  than panicking on out-of-range indices. Device data is untrusted input;
  never index a `&[u32]` directly in new decode code.
- **Fixtures are ground truth.** `crates/gocoax/tests/fixtures/*.json` are
  real captured `/ms/` response bodies from a live device (see also
  `docs/superpowers/reference/fixtures/`, the original captures those were
  sourced from). Golden test values were checked against the device's own
  rendered UI, not invented. If you change a decoder, re-verify against these
  fixtures before trusting new output.
- **Secrets are never committed.** `.credentials` and `config.toml` (plus
  `config.local.toml`) are git-ignored (see `.gitignore`). Use
  `config.example.toml` as the template. Credentials resolve as
  `password` (inline) → `password_env` (env var name) → `password_file`
  (trimmed file contents), with per-`[[device]]` overrides of the global
  `username`/password source (`config.rs::resolve_password`/`creds_for`).

## Out of scope / future

- **Phase 3 — auto-remediation: BUILT** as `gocoax-remediator` (option B, the
  self-deciding daemon — it polls Prometheus, so the "sustained" hysteresis
  lives in each rule's PromQL, no Alertmanager needed). Reboots on configurable
  rules with a per-device cooldown + daily circuit breaker; `dry_run = true` by
  default. See `docs/remediator.md`. Follow-ups noted there: in-memory state
  resets on restart; `circuit_open` can stay stale if a device fully stops
  matching any rule.
- **Phase 4 — firmware upgrade.** The device's `upgrade.html` (bulk
  flashing) is **not mapped or implemented** — highest-risk, deferred,
  needs its own spec.

## Known limitations (all-MoCA-2.5 simplifications)

This implementation targets all-MoCA-2.5 hardware and makes several simplifications
that would require refactoring for mixed/1.x networks:

- **`Client::phy_rates` network-wide `final_ver`**: derives `final_ver` from
  `localInfo[11]` across the entire network, whereas the device firmware uses a
  per-node `min(nc_moca_ver, node_moca_ver)`-derived value. On a mixed 1.x/2.x
  network, an FMR request would use the wrong payload version; the device silently
  returns a zeroed payload starting around word 10 (see "PHY rate" in *Decode
  gotchas* above).
- **`PhyRates.gcd_mbps` (self/diagonal)**: reuses the self-link NPER value for
  GCD. The device computes GCD from a separate formula that diverges for an
  exact-MoCA-2.0 node (where `gapVLper==0`) and for a 1.x self-node (50 MHz
  formula).
- **`nc_node_id` lacks masking**: read directly from `localInfo[1]` without the
  `& 0xff` mask the device JS applies. Harmless for node IDs 0–15 (all real
  hardware), but semantically incomplete.
- **Absent-node-skip, 1.x unpack, and mixed-network FMR branches**: ported from
  the device JS but untested — no fixture exists for mixed hardware. Validate via
  browser cross-check when running on such hardware.

## Validation approach

Curl/direct HTTP against the device pages shows **empty templates** — the
actual field values are computed client-side in `main.js`/`phyRates.html` JS
after the page loads and calls `Refresh()`. To cross-check a decoder against
what the device UI actually displays, you must **render the page in a real
browser** (e.g. the Claude Chrome extension or chrome-devtools MCP), not just
fetch its HTML. This was the acceptance-gate method used to validate the PHY
rate formulas (`701`/`3656` Mbps matched pixel-for-pixel against the UI) and
is the recommended way to add new fixtures or catch decode drift when
pointing this at new/different hardware.

## Build / run / test

```bash
cargo build --release              # builds target/release/{gocoax,gocoax-exporter}
cargo test --workspace             # all unit + integration tests (fixtures, mock server, exporter render)

cp config.example.toml config.toml # then fill in devices + credentials
./target/release/gocoax status --config config.toml --device <name>
./target/release/gocoax reboot --config config.toml --device <name> --yes   # refuses without --yes
./target/release/gocoax discover --http 192.0.2.0/24    # or --mac, or --self-report --config ... --device ...
./target/release/gocoax-exporter --config config.toml      # serves GET /metrics on `listen` (default 0.0.0.0:9420)
```

## Pointers

- Design spec (protocol, decisions, full metrics catalog, future phases):
  `docs/superpowers/specs/2026-08-08-gocoax-tools-design.md`
- Device page JS — the **decode source of truth** these decoders port from:
  `docs/superpowers/reference/device-pages/` (`devStatus.html`,
  `phyRates.html`, `main.js`)
- Captured raw `/ms/` responses (origin of the test fixtures):
  `docs/superpowers/reference/fixtures/`
- `README.md` (repo root) — user-facing build/config/run instructions and
  full config field reference.
- `grafana-dashboard.json` — importable dashboard covering all metrics below.
