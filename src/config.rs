use std::{net::Ipv4Addr, path::PathBuf};

use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use serde::{Deserialize, Serialize};

use crate::{errors::ServeError, tls::Tls};

#[derive(Subcommand, Debug, Serialize, Deserialize, Clone)]
pub enum Subcommands {
    /// Adds TLS support
    Tls(Tls),
}

const CONFIG_HELP: &str = r#"Path to the configuration file.
Command line arguments override the configuration file.
If configuration file does not exist, it will be created with the current
command line arguments.

Supported format is TOML.
"#;

const LOG_PATH_HELP: &str = r#"Path to the directory where logs will be stored.
If not specified, logs will be printed to stdout.
If specified, logs will be written to the file: log_path/serve.YYYY-MM-DD.log
and rotated daily.
If the directory does not exist, it will be created.
"#;

#[derive(Parser, Debug, Serialize, Deserialize, Clone)]
#[command(version, about, long_about = None)]
pub struct ServeArgs {
    #[clap(short, long, help = CONFIG_HELP)]
    pub config: Option<PathBuf>,
    /// Subcommands for additional features.
    #[clap(subcommand)]
    pub subcommand: Option<Subcommands>,
    /// Path to the directory to serve. Defaults to the current directory.
    #[clap(long)]
    pub path: Option<PathBuf>,
    /// Port to listen on.
    #[clap(short, long, default_value_t = 3000)]
    pub port: u16,
    /// Address to listen on.
    #[clap(short, long, default_value = "127.0.0.1")]
    pub addr: Ipv4Addr,
    /// Compression layer is enabled by default.
    #[clap(long)]
    pub disable_compression: bool,
    /// Path to 404 page. By default, 404 is empty.
    #[clap(long)]
    pub not_found: Option<PathBuf>,
    /// Override with 200 OK. Useful for SPA. Requires --not-found.
    #[clap(long, requires = "not_found")]
    pub ok: bool,
    /// Log level.
    #[command(flatten)]
    pub log_level: Verbosity,
    #[clap(long, help = LOG_PATH_HELP)]
    pub log_path: Option<PathBuf>,
    /// Maximum number of log files to keep.
    #[clap(long, requires = "log_path", default_value = "7")]
    pub log_max_files: Option<usize>,
}

impl ServeArgs {
    pub fn get_path(&self) -> PathBuf {
        self.path.clone().unwrap_or(".".into())
    }

    pub fn resolve_config(self) -> Result<Self, ServeError> {
        let Some(config_path) = self.config.as_ref() else {
            return Ok(self);
        };

        if !config_path.exists() {
            self.write_config()?;
        }

        if !config_path.is_file() {
            return Err(ServeError::InvalidPath(format!(
                "Configuration path is not a file: {}",
                config_path.display()
            )));
        }

        let content = std::fs::read_to_string(config_path)?;
        let config: Self = toml::from_str(&content)?;

        Ok(Self {
            config: Some(config_path.clone()),
            subcommand: config.subcommand,
            path: self.path.or(config.path),
            port: config.port,
            addr: config.addr,
            disable_compression: self.disable_compression || config.disable_compression,
            not_found: self.not_found.or(config.not_found),
            ok: self.ok || config.ok,
            log_level: if self.log_level.is_present() {
                self.log_level
            } else {
                config.log_level
            },
            log_path: self.log_path.or(config.log_path),
            log_max_files: self.log_max_files.or(config.log_max_files),
        })
    }

    fn write_config(&self) -> Result<(), ServeError> {
        let config_path = self
            .config
            .as_ref()
            .ok_or_else(|| ServeError::GenerateConfig("No config path specified".to_string()))?;

        let mut config = self.clone();
        config.config = None;

        config.path = config.path.map(|p| p.canonicalize().unwrap_or(p));
        config.not_found = config.not_found.map(|p| p.canonicalize().unwrap_or(p));
        config.log_path = config.log_path.map(|p| p.canonicalize().unwrap_or(p));

        config.subcommand = match config.subcommand {
            Some(Subcommands::Tls(ref tls)) => Some(Subcommands::Tls(Tls {
                cert: tls.cert.canonicalize().unwrap_or(tls.cert.clone()),
                key: tls.key.canonicalize().unwrap_or(tls.key.clone()),
                redirect_http: tls.redirect_http,
            })),
            None => None,
        };

        let toml = format!(
            "# Configuration for Serve (https://github.com/Lurk/serve)\n\n{}",
            toml::ser::to_string_pretty(&config)?
        );
        std::fs::write(config_path, toml)?;
        println!(
            "Configuration file created at: {0}\nYou can run\nserve --config {0}\nto use it.\n",
            config_path.canonicalize()?.display()
        );
        Ok(())
    }
}
