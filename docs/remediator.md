# gocoax-remediator — automatic reboot on sustained problems

A small daemon that watches the metrics the exporter produces and **reboots
adapters that are stuck**, then records every reboot as its own Prometheus
metrics so the history shows up in Grafana.

It reuses two things that already exist: the health metrics from
`gocoax-exporter`, and `gocoax::Client::reboot()` (the `/ms/1/0xb00` call).

## How it works

```
Prometheus (metric history)  ──query──▶  gocoax-remediator  ──reboot──▶  adapter
        ▲                                      │
        └───────── scrapes ────────────────────┘  (its own /metrics: reboot counters)
```

Every `poll_interval_secs`, for each configured rule it runs the rule's PromQL
against Prometheus. Any series the query returns that carries a `device` label
is considered problematic; that device is rebooted, subject to the safety
limits. Using PromQL means the "sustained" logic (hysteresis) lives in the
query itself and reuses Prometheus's stored history — no Alertmanager needed.

## Triggers (configurable PromQL rules)

Defaults — each is written so it only matches a *sustained* condition:

| Rule | Default expr | Meaning |
|---|---|---|
| `unreachable` | `max_over_time(gocoax_up[10m]) == 0` | device read failed for 10m straight |
| `link_down` | `max_over_time(gocoax_moca_link_up[10m]) == 0` | MoCA link down for 10m straight |
| `rx_drops` | `avg_over_time(rate(gocoax_ethernet_rx_frames_total{status="dropped"}[5m])[15m:]) > 0.001` | chronic Ethernet rx drops (healthy adapters read exactly 0) |

Any rule can be edited/added/removed in config. The rule `name` becomes the
`reason` label on the reboot metrics.

## Safety (always on, even in auto mode)

- **Cooldown** (`cooldown_secs`, default 1800 = 30 min): a device won't be
  rebooted again until this elapses, no matter how many rules match.
- **Circuit breaker** (`max_reboots_per_day`, default 4): after N reboots of a
  device in a day, stop rebooting it and set `gocoax_remediator_circuit_open=1`
  so a human gets alerted instead of an infinite loop.
- **`dry_run`**: when true, it logs "would reboot" and increments
  `gocoax_remediator_would_reboot_total` **without actually rebooting**.
  **Defaults to `true`** (fails closed) — omitting the key from config never
  silently enables live reboots; a user must explicitly set `dry_run = false`
  to enable fully-automatic operation. The cooldown and daily circuit breaker
  apply identically whether `dry_run` is true or false, so the dry-run
  preview reflects the same cadence live mode would produce.
- **Failed reboot attempts still consume cooldown/breaker budget**: an
  attempt (live, not dry-run) that fails is recorded the same as a
  successful one for cooldown/breaker purposes — so a device that's
  actually unreachable doesn't get hammered with a reboot POST every poll.
  The failure itself is reported via `gocoax_remediator_reboot_failures_total`
  rather than `gocoax_remediator_reboots_total`.

## Metrics it exposes (→ Prometheus → Grafana history)

```
gocoax_remediator_up                                     1 = daemon healthy
gocoax_remediator_reboots_total{device,reason}           counter of reboots performed successfully
gocoax_remediator_would_reboot_total{device,reason}      counter of reboots suppressed by dry_run
gocoax_remediator_reboot_failures_total{device,reason}   counter of reboot attempts that failed
gocoax_remediator_last_reboot_timestamp_seconds{device}  unix ts of last *successful* reboot
gocoax_remediator_circuit_open{device}                   1 = breaker tripped, no longer rebooting
```

The three `*_total` counters and `circuit_open` are **zero-initialized at
startup** for every configured `device × rule` pair, so the series exist at 0
*before* the first event. This matters because Prometheus's
`rate()`/`increase()`/`changes()` cannot see a counter's first increment if the
series is *born* at 1 (there's no prior 0 sample to diff against) — so without
seeding, a device's very first reboot would be invisible in every "increase"
panel and draw no annotation. `last_reboot_timestamp_seconds` is deliberately
*not* seeded (a 0 would render as "rebooted at the Unix epoch").

Grafana: add an **annotation query** on `changes(gocoax_remediator_reboots_total[$__range]) > 0`
(or `increase(...[1m]) > 0`) so each reboot draws a vertical marker across the
dashboard, plus a "reboots (24h)" table from `increase(gocoax_remediator_reboots_total[24h])`.
Because of the zero-init above, both fire correctly on the *first* reboot too.

## Config (extends the same `config.toml`)

Reuses the existing `[[device]]` list + credentials, plus one `[remediator]`
table:

```toml
[remediator]
prometheus_url = "http://prometheus:9090"
poll_interval_secs = 60
cooldown_secs = 1800
max_reboots_per_day = 4
listen = "0.0.0.0:9421"
dry_run = true            # start true; set false for fully-automatic reboots

[[remediator.rule]]
name = "unreachable"
expr = 'max_over_time(gocoax_up[10m]) == 0'
[[remediator.rule]]
name = "link_down"
expr = 'max_over_time(gocoax_moca_link_up[10m]) == 0'
[[remediator.rule]]
name = "rx_drops"
expr = 'avg_over_time(rate(gocoax_ethernet_rx_frames_total{status="dropped"}[5m])[15m:]) > 0.001'
```

## Logging / visibility

By default the daemon is quiet: it logs startup, every reboot / would-reboot /
failure, and warnings (a rule query that failed, or a matched device missing
from config). It does **not** log routine "nothing wrong" polls or cooldown
skips — so on a healthy system `docker logs` is nearly silent after startup.

Set **`verbose = true`** in `[remediator]` to log each poll cycle and the
per-device decision — the matched device set per rule, and for each matched
device whether it's rebooting, in cooldown (with seconds left), or blocked by
the circuit breaker. Handy while tuning thresholds or watching a dry-run:

```
gocoax-remediator: poll: evaluating 3 rule(s) [dry-run]
gocoax-remediator:   rule 'rx_drops' matched 1 device(s): ["master-bedroom-moca"]
gocoax-remediator:     device=master-bedroom-moca reason=rx_drops -> in cooldown (1140s left), skipping
```

The always-on `/metrics` counters (`would_reboot_total`, `reboots_total`, …)
are the durable record; verbose logging is for real-time observation.

## State

Cooldown / daily-count state is in-memory (resets on restart — acceptable for a
home setup; a restart just clears the daily counter). Reboot counters are
Prometheus counters, so their long-term history lives in Prometheus regardless
of restarts: on restart the in-memory counter drops back to 0, which Prometheus
treats as a normal **counter reset** — `rate()`/`increase()` stitch across it,
and already-scraped samples are never deleted. So `increase(...[24h])` remains
the correct, restart-safe way to ask "how many reboots"; only the raw
instantaneous counter value restarts from 0 (true of every Prometheus counter on
process restart). The narrow loss window: a reboot in the seconds between its
counter bump and the next Prometheus scrape, if the daemon then crashes, is
absent from the counter (but still in the logs) — persisting state to disk would
be the only fix, deferred as overkill for a home setup.
