use clap::{Parser, Subcommand};
use gocoax::client::{Client, ClientOpts};
use gocoax::config::{Config, Device};
use gocoax::discover;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "gocoax")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Status {
        #[arg(long)]
        config: String,
        /// Device name from the config. Omit to report ALL devices in the config.
        #[arg(long)]
        device: Option<String>,
    },
    Reboot { #[arg(long)] config: String, #[arg(long)] device: String, #[arg(long)] yes: bool },
    Discover {
        /// HTTP fingerprint scan of a CIDR, e.g. 192.0.2.0/24
        #[arg(long)]
        http: Option<String>,
        /// Filter the system ARP table by known MoCA OUIs
        #[arg(long)]
        mac: bool,
        /// MoCA self-report via one authenticated adapter (needs --config + --device)
        #[arg(long)]
        self_report: bool,
        #[arg(long)]
        config: Option<String>,
        #[arg(long)]
        device: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Status { config, device } => {
            let cfg = load_config(&config)?;
            // A specific --device reports just that one (error if unknown);
            // omitting --device reports every device in the config.
            let selected: Vec<&Device> = match &device {
                Some(name) => vec![cfg
                    .device
                    .iter()
                    .find(|d| &d.name == name)
                    .ok_or_else(|| format!("device {name} not in config"))?],
                None => cfg.device.iter().collect(),
            };
            if selected.is_empty() {
                eprintln!("no devices in config");
                std::process::exit(2);
            }
            let all_mode = device.is_none();
            let mut failures = 0;
            for dev in selected {
                if all_mode {
                    println!("===== {} ({}) =====", dev.name, dev.host);
                }
                match status_one(&cfg, dev).await {
                    Ok(s) => println!("{s:#?}"),
                    // In all-devices mode, one adapter failing must not abort
                    // the rest — report it and keep going.
                    Err(e) => {
                        eprintln!("  error: {e}");
                        failures += 1;
                    }
                }
            }
            // Non-zero exit if any device failed (useful in scripts).
            if failures > 0 {
                std::process::exit(1);
            }
        }
        Cmd::Reboot { config, device, yes } => {
            if !yes {
                eprintln!("refusing to reboot {device} without --yes");
                std::process::exit(2);
            }
            let (_cfg, client) = build(&config, &device)?;
            client.reboot().await?;
            println!("reboot sent to {device}");
        }
        Cmd::Discover { http, mac, self_report, config, device } => {
            if let Some(cidr) = http {
                let hosts = discover::cidr_hosts(&cidr)?;
                let found = discover::http_fingerprint(&hosts, 800, 1500, 64).await;
                for f in &found {
                    println!("{}\t{}", f.ip, f.server.as_deref().unwrap_or(""));
                }
            } else if mac {
                let found = discover::mac_filter(discover::MOCA_OUIS)?;
                for f in &found {
                    println!("{}\t{}", f.ip, f.mac.as_deref().unwrap_or(""));
                }
            } else if self_report {
                let (Some(config), Some(device)) = (config, device) else {
                    eprintln!("--self-report requires --config and --device");
                    std::process::exit(2);
                };
                let (_cfg, client) = build(&config, &device)?;
                let nodes = client.moca_nodes().await?;
                for n in &nodes {
                    println!("{n:?}");
                }
            } else {
                eprintln!("discover: specify one of --http <cidr>, --mac, or --self-report");
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

fn load_config(config_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(config_path)?;
    Ok(Config::from_toml(&text)?)
}

fn client_for(cfg: &Config, dev: &Device) -> Result<Client, Box<dyn std::error::Error>> {
    let creds = cfg.creds_for(dev)?;
    let opts = ClientOpts {
        request_timeout: Duration::from_secs(cfg.request_timeout_secs),
        connect_timeout: Duration::from_secs(cfg.connect_timeout_secs),
    };
    Ok(Client::new(&dev.host, creds, opts)?)
}

async fn status_one(
    cfg: &Config,
    dev: &Device,
) -> Result<gocoax::DeviceStatus, Box<dyn std::error::Error>> {
    let client = client_for(cfg, dev)?;
    Ok(client.device_status().await?)
}

fn build(config_path: &str, device: &str) -> Result<(Config, Client), Box<dyn std::error::Error>> {
    let cfg = load_config(config_path)?;
    let dev = cfg
        .device
        .iter()
        .find(|d| d.name == device)
        .ok_or_else(|| format!("device {device} not in config"))?;
    let client = client_for(&cfg, dev)?;
    Ok((cfg, client))
}
