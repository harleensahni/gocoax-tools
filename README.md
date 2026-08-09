# gocoax-tools

Lightweight Rust tooling to observe GoCoax MoCA adapters (MaxLinear MXL371x
family) over their web management interface, and expose their stats as
Prometheus metrics for Grafana.

The workspace has three crates:

- **`gocoax`** (`crates/gocoax`) — core library: an async client for the
  device's register-read HTTP protocol, typed decoders for device status /
  Ethernet counters / PHY rates, and `reboot()`. Also builds a small **`gocoax`
  CLI binary** (`crates/gocoax/src/bin/gocoax.rs`) for exercising the library
  by hand.
- **`gocoax-exporter`** (`crates/gocoax-exporter`) — a Prometheus exporter
  binary that polls the devices in your config and serves `/metrics`.
- **`gocoax-remediator`** (`crates/gocoax-remediator`) — an optional daemon that
  watches the exporter's metrics via Prometheus and automatically reboots stuck
  adapters, gated by a per-device cooldown + daily circuit breaker and
  `dry_run = true` by default. See [Automatic remediation](#automatic-remediation-optional)
  below and [`docs/remediator.md`](docs/remediator.md).

## Build

```bash
cargo build --release
```

Produces:

- `target/release/gocoax` — the CLI
- `target/release/gocoax-exporter` — the exporter
- `target/release/gocoax-remediator` — the optional auto-reboot daemon
  (see [Automatic remediation](#automatic-remediation-optional))

## Configure

Copy the example config and fill in your devices:

```bash
cp config.example.toml config.toml
```

`config.toml` is git-ignored (it can contain credentials), and is the file
both `gocoax` and `gocoax-exporter` expect via `--config`.

```toml
username = "admin"
# password = "..."          # or use password_env / password_file
# password_env = "GOCOAX_PW"
listen = "0.0.0.0:9420"
request_timeout_secs = 8
connect_timeout_secs = 3
scrape_deadline_secs = 9

[[device]]
name = "moca-1"
host = "192.0.2.250"
```

Fields:

- `username` / `password` / `password_env` / `password_file` — credentials
  used for HTTP Basic Auth against the device's web UI. Exactly one password
  source is needed: an inline `password`, an environment variable name via
  `password_env`, or a file path via `password_file` (its contents are
  trimmed and used as the password).
- `listen` — address the exporter's `/metrics` server binds to. Default
  `0.0.0.0:9420`.
- `request_timeout_secs` — per-HTTP-request timeout. Default `8`.
- `connect_timeout_secs` — TCP connect timeout. Default `3`.
- `scrape_deadline_secs` — global cap on a single `/metrics` scrape across
  *all* devices (see below). Default `9`.
- `[[device]]` — one entry per adapter: `name` (used as the Prometheus
  `device` label), `host` (IP or hostname), and optional per-device
  `username` / `password` / `password_env` / `password_file` overrides. A
  device without its own credentials inherits the global ones.

## Run the exporter

```bash
./target/release/gocoax-exporter --config config.toml
```

This starts an HTTP server on `config.listen` (default `0.0.0.0:9420`) with a
single endpoint, `GET /metrics`.

Each request to `/metrics` triggers a fresh, concurrent poll of every
configured device (one task per device), bounded by `scrape_deadline_secs`
overall — there's no background polling loop or stale cache; Prometheus's own
`scrape_interval` drives the cadence.

Device failures are isolated: a device that's unreachable, timing out, or
failing auth reports `gocoax_up{device="..."} 0` plus a classified
`gocoax_scrape_errors_total{device,reason}` count. **`/metrics` always returns
200**, even if every device is down — a dead device is data, not an exporter
error.

### Prometheus scrape config

```yaml
scrape_configs:
  - job_name: gocoax
    static_configs:
      - targets: ["<exporter-host>:9420"]
```

## Running as a container

The exporter ships as a tiny (~2.5 MB) static image on `scratch`. **Run it on a
host that can reach your adapters' LAN** (e.g. the same box as Grafana); your
Prometheus server can live anywhere and just scrapes `<that-host>:9420`.

Everything below works identically with **docker** or **podman** (swap the
command name).

**Pull and run the published image:**

```bash
# put your config next to you; the password can come from an env var so it's
# not baked into any file — set `password_env = "GOCOAX_PW"` in config.toml
docker run -d --name gocoax-exporter --restart unless-stopped \
  -p 9420:9420 \
  -e GOCOAX_PW="your-device-password" \
  -v "$PWD/config.toml:/etc/gocoax/config.toml:ro" \
  ghcr.io/harleensahni/gocoax-tools:latest
```

(`podman run …` is identical.) The image is published by the
`build-and-publish-image` GitHub Actions workflow on every push to `main`.

**Or build it locally** (works on any arch — builds a static binary for the
host's architecture):

```bash
docker build -t gocoax-exporter .
docker run -d --restart unless-stopped -p 9420:9420 \
  -e GOCOAX_PW="your-device-password" \
  -v "$PWD/config.toml:/etc/gocoax/config.toml:ro" \
  gocoax-exporter
```

**Or with compose** (`docker compose up -d` / `podman-compose up -d`) — see
[`compose.yaml`](compose.yaml).

Then point Prometheus (wherever it runs) at `<container-host>:9420` using the
scrape config above. Check it's working: `curl <container-host>:9420/metrics`.

## CLI

The `gocoax` binary talks to one device directly (mainly for ad-hoc checks
and testing), reading the same `config.toml` for device host/credentials:

```bash
# Print a device's decoded status (SOC/MoCA versions, link state, node
# count, MAC/IP, Ethernet counters and port state).
gocoax status --config config.toml --device moca-1

# Omit --device to report EVERY device in the config. Each device is printed
# under a "===== name (host) =====" header; one adapter failing does not abort
# the rest (its error is printed and the command exits non-zero).
gocoax status --config config.toml

# Reboot a device. Requires --yes; without it the command refuses and exits
# non-zero rather than rebooting.
gocoax reboot --config config.toml --device moca-1 --yes
```

### Discovering adapters on the LAN

`gocoax discover` finds MoCA adapters on your network three ways (use one mode
per invocation):

```bash
# 1. HTTP fingerprint scan of a subnet — matches the adapters' InterNiche
#    webserver signature. No credentials needed. Fast (a /24 in a few seconds).
gocoax discover --http 192.0.2.0/24

# 2. Filter the system ARP table by known MoCA vendor OUIs (e.g. 94:cc:04).
#    Handy when the adapters are already in your ARP cache.
gocoax discover --mac

# 3. MoCA self-report: ask one authenticated adapter to enumerate every node
#    on its coax network (node id, MAC, MoCA version).
gocoax discover --self-report --config config.toml --device moca-1
```

## Grafana dashboard

Import `grafana-dashboard.json` to get a starter dashboard ("GoCoax MoCA"):

1. In Grafana: **Dashboards → New → Import**.
2. Upload `grafana-dashboard.json` (or paste its contents).
3. When prompted, pick your Prometheus datasource for the `datasource`
   variable.
4. Click **Import**.

The dashboard has two template variables:

- `$datasource` — the Prometheus datasource to query.
- `$device` — multi-select (with "All") over `label_values(gocoax_up,
  device)`, used to filter every panel to one or more configured devices.

Panels cover device health (`up` + last-good-read age), the PHY rate matrix,
MoCA link/node state, Ethernet link/speed and full TX/RX frame-rate breakdown
(good/bad/dropped), exporter scrape health, a **Remediation** section (reboots,
circuit-breaker state, time-since-last-reboot, failures/would-reboot), and a
device inventory table that also carries per-device TX/RX frame totals. Reboots
appear as red annotations across every panel.

## Automatic remediation (optional)

`gocoax-remediator` watches the exporter's metrics via Prometheus and **reboots
adapters that are stuck** (unreachable, link-down, or chronic Ethernet drops),
recording every reboot as its own metrics so the history shows in Grafana. Full
design + tuning: [`docs/remediator.md`](docs/remediator.md).

It's the third binary in the same image. Run it alongside the exporter — the
`compose.yaml` has a `gocoax-remediator` service, or:

```bash
docker run -d --name gocoax-remediator --restart unless-stopped \
  -p 9421:9421 \
  -e GOCOAX_PW='your-device-password' \
  -v "$PWD/config.toml:/etc/gocoax/config.toml:ro" \
  --entrypoint /gocoax-remediator \
  ghcr.io/harleensahni/gocoax-tools:latest
```

Configure it with the `[remediator]` block in `config.toml` (see
`config.example.toml`) and scrape it from Prometheus:

```yaml
  - job_name: gocoax_remediator
    static_configs:
      - targets: ["<remediator-host>:9421"]
```

> **Safety:** it defaults to **`dry_run = true`** — it only logs "would reboot"
> and increments `gocoax_remediator_would_reboot_total` until you explicitly set
> `dry_run = false`. A per-device **cooldown** and daily **circuit breaker**
> apply in both modes. Start in dry-run, watch what it *would* do, then enable.

## Metric catalog

Common label: `device` (the config's `[[device]].name`). `gocoax_info` also
carries `host`, `mac`, `ip`, `soc_version`, `moca_version`.

### Health (two-layer `up`)

There are two independent layers of "is this working":

1. **`up{job="gocoax"}`** — a metric Prometheus generates automatically for
   every scrape target. `1` means the exporter process itself answered
   `/metrics`; `0` means Prometheus couldn't reach the exporter at all (it's
   down, unreachable, or the request errored/timed out at the HTTP level).
2. **`gocoax_up{device="..."}`** — emitted *by* the exporter, per configured
   device. `1` means that specific MoCA adapter answered a status read on
   this scrape; `0` means it didn't (device offline, auth failure, timeout,
   etc.). This can be `0` while `up{job="gocoax"}` is `1` — the exporter is
   healthy, one of its devices isn't.

| Metric | Labels | Meaning |
|---|---|---|
| `gocoax_up` | `device` | `1`/`0` — current device read health |
| `gocoax_scrape_errors_total` | `device`, `reason` | Counter; `reason` = `unreachable`\|`timeout`\|`auth`\|`csrf`\|`http_status`\|`decode` |
| `gocoax_last_success_timestamp_seconds` | `device` | Unix timestamp of the device's last fully successful scrape |
| `gocoax_scrape_duration_seconds` | `device` | Per-device scrape duration |

### Device data (only emitted for devices that are currently up)

| Metric | Labels | Meaning |
|---|---|---|
| `gocoax_info` | `device`, `host`, `mac`, `ip`, `soc_version`, `moca_version` | Always `1`; a label carrier for device identity/firmware |
| `gocoax_moca_link_up` | `device` | `1`/`0` — MoCA link state |
| `gocoax_moca_nodes` | `device` | Number of nodes on the MoCA network |
| `gocoax_node_moca_version` | `device`, `node` | Per-node MoCA protocol version (e.g. `25` = 2.5, `20` = 2.0) |
| `gocoax_phy_rate_mbps` | `device`, `from_node`, `to_node`, `type` | PHY rate between a node pair; `type` = `nper`\|`vlper` (a self pair is the per-node rate) |
| `gocoax_phy_rate_gcd_mbps` | `device`, `node` | Per-node GCD PHY rate |
| `gocoax_ethernet_link_up` | `device`, `port` | `1`/`0` — Ethernet port link state |
| `gocoax_ethernet_speed_mbps` | `device`, `port` | Negotiated Ethernet port speed |
| `gocoax_ethernet_tx_frames_total` | `device`, `port`, `status` | Counter; `status` = `good`\|`bad`\|`dropped` |
| `gocoax_ethernet_rx_frames_total` | `device`, `port`, `status` | Counter; `status` = `good`\|`bad`\|`dropped` |

Counters end in `_total` and are monotonic, so `rate()`/`increase()` in
Grafana/PromQL show error spikes cleanly.

## License

Licensed under the [MIT License](LICENSE).
