use std::path::{Path, PathBuf};

use crate::config::{InitArgs, InstallArgs, ServeArgs, ValidateArgs};
use crate::errors::ServeError;

const BINARY_INSTALL_PATH: &str = "/usr/local/bin/serve";

#[cfg(target_os = "linux")]
const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/serve.service";

#[cfg(target_os = "macos")]
const LAUNCHD_PLIST_PATH: &str = "/Library/LaunchDaemons/com.serve.plist";

#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.serve";

#[cfg(target_os = "linux")]
const SERVICE_USER: &str = "serve";

fn run_command(cmd: &str, args: &[&str]) -> Result<String, ServeError> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| ServeError::Service(format!("Failed to execute {cmd}: {e}")))?;

    if !output.status.success() {
        return Err(ServeError::CommandFailed {
            command: format!("{} {}", cmd, args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_command_ok(cmd: &str, args: &[&str]) -> String {
    run_command(cmd, args).unwrap_or_default()
}

fn require_root() -> Result<(), ServeError> {
    let uid = run_command("id", &["-u"])?;
    if uid.trim() != "0" {
        return Err(ServeError::Service(
            "This command must be run as root (use sudo)".to_string(),
        ));
    }
    Ok(())
}

fn default_config_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/serve/serve.toml")
    } else {
        PathBuf::from("/etc/serve/serve.toml")
    }
}

fn discover_config_path() -> Result<PathBuf, ServeError> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string(SYSTEMD_UNIT_PATH).map_err(|_| {
            ServeError::Service(
                "Service is not installed. Could not read systemd unit file.".to_string(),
            )
        })?;
        for line in content.lines() {
            let line = line.trim();
            if let Some(exec) = line.strip_prefix("ExecStart=") {
                let parts: Vec<&str> = exec.split_whitespace().collect();
                for (i, part) in parts.iter().enumerate() {
                    if *part == "-c" || *part == "--config" {
                        if let Some(path) = parts.get(i + 1) {
                            return Ok(PathBuf::from(path));
                        }
                    }
                }
            }
        }
        Err(ServeError::Service(
            "Could not find config path in systemd unit file".to_string(),
        ))
    }

    #[cfg(target_os = "macos")]
    {
        let content = std::fs::read_to_string(LAUNCHD_PLIST_PATH).map_err(|_| {
            ServeError::Service(
                "Service is not installed. Could not read launchd plist.".to_string(),
            )
        })?;
        let lines: Vec<&str> = content.lines().map(str::trim).collect();
        for (i, line) in lines.iter().enumerate() {
            if line.contains("-c") && line.contains("<string>") {
                if let Some(next) = lines.get(i + 1) {
                    if let Some(path) = next.strip_prefix("<string>") {
                        if let Some(path) = path.strip_suffix("</string>") {
                            return Ok(PathBuf::from(path));
                        }
                    }
                }
            }
        }
        Err(ServeError::Service(
            "Could not find config path in launchd plist".to_string(),
        ))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ServeError::Service(
            "Unsupported platform for service management".to_string(),
        ))
    }
}

fn generate_default_config() -> String {
    r#"# Configuration for Serve (https://github.com/Lurk/serve)

path = "/var/www/serve"
port = 3000
addr = "127.0.0.1"
disable_compression = false
ok = false
log_level = "info"
log_path = "/var/log/serve"
log_max_files = 7
"#
    .to_string()
}

#[cfg(target_os = "linux")]
fn generate_systemd_unit(config_path: &Path) -> String {
    format!(
        r"[Unit]
Description=Serve - Static File Server
After=network.target

[Service]
Type=simple
User={user}
Group={user}
ExecStart={binary} -c {config}
WorkingDirectory=/var/www/serve
Restart=on-failure
RestartSec=5

AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

ProtectSystem=strict
ProtectHome=read-only
NoNewPrivileges=false
ReadWritePaths=/var/log/serve /var/www/serve
ReadOnlyPaths=/etc/letsencrypt

[Install]
WantedBy=multi-user.target
",
        user = SERVICE_USER,
        binary = BINARY_INSTALL_PATH,
        config = config_path.display(),
    )
}

#[cfg(target_os = "macos")]
fn generate_launchd_plist(config_path: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>-c</string>
        <string>{config}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>/var/www/serve</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/serve/serve.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/serve/serve.stderr.log</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        binary = BINARY_INSTALL_PATH,
        config = config_path.display(),
    )
}

fn create_dir_if_needed(path: &str) -> Result<(), ServeError> {
    let p = Path::new(path);
    if !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

pub fn init(args: &InitArgs) -> Result<(), ServeError> {
    let config_path = args.path.clone().unwrap_or_else(default_config_path);

    if config_path.exists() {
        return Err(ServeError::Service(format!(
            "Config file already exists: {}",
            config_path.display()
        )));
    }

    if let Some(parent) = config_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    std::fs::write(&config_path, generate_default_config())?;
    println!("Configuration file created at: {}", config_path.display());
    Ok(())
}

pub fn install(args: &InstallArgs) -> Result<(), ServeError> {
    require_root()?;

    let config_path = args.config.canonicalize().map_err(|_| {
        ServeError::Service(format!("Config file not found: {}", args.config.display()))
    })?;

    // Validate config
    let content = std::fs::read_to_string(&config_path)?;
    let _config: ServeArgs = toml::from_str(&content)
        .map_err(|e| ServeError::Service(format!("Invalid config: {e}")))?;
    println!("Config validated: {}", config_path.display());

    // Copy binary
    let current_exe = std::env::current_exe().map_err(|e| {
        ServeError::Service(format!("Could not determine current binary path: {e}"))
    })?;
    let install_path = Path::new(BINARY_INSTALL_PATH);
    if install_path.exists() {
        std::fs::remove_file(install_path)?;
    }
    std::fs::copy(&current_exe, BINARY_INSTALL_PATH)?;
    println!("Binary installed to {BINARY_INSTALL_PATH}");

    // Create directories
    create_dir_if_needed("/var/log/serve")?;
    create_dir_if_needed("/var/www/serve")?;

    #[cfg(target_os = "linux")]
    {
        // Create service user (ignore error if already exists)
        run_command_ok(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                SERVICE_USER,
            ],
        );

        // Set directory ownership
        run_command_ok(
            "chown",
            &[&format!("{SERVICE_USER}:{SERVICE_USER}"), "/var/log/serve"],
        );
        run_command_ok(
            "chown",
            &[&format!("{SERVICE_USER}:{SERVICE_USER}"), "/var/www/serve"],
        );

        // Write systemd unit
        let unit = generate_systemd_unit(&config_path);
        std::fs::write(SYSTEMD_UNIT_PATH, unit)?;
        println!("Systemd unit installed to {SYSTEMD_UNIT_PATH}");

        // Enable and start
        run_command("systemctl", &["daemon-reload"])?;
        run_command("systemctl", &["enable", "serve.service"])?;
        run_command("systemctl", &["start", "serve.service"])?;
        println!("Service enabled and started");
    }

    #[cfg(target_os = "macos")]
    {
        // Write launchd plist
        let plist = generate_launchd_plist(&config_path);
        std::fs::write(LAUNCHD_PLIST_PATH, plist)?;
        println!("Launchd plist installed to {LAUNCHD_PLIST_PATH}");

        // Bootstrap (try modern API first, fall back to legacy)
        if run_command("launchctl", &["bootstrap", "system", LAUNCHD_PLIST_PATH]).is_err() {
            run_command("launchctl", &["load", "-w", LAUNCHD_PLIST_PATH])?;
        }
        println!("Service loaded and started");
    }

    Ok(())
}

pub fn uninstall() -> Result<(), ServeError> {
    require_root()?;

    #[cfg(target_os = "linux")]
    {
        run_command_ok("systemctl", &["stop", "serve.service"]);
        run_command_ok("systemctl", &["disable", "serve.service"]);

        if Path::new(SYSTEMD_UNIT_PATH).exists() {
            std::fs::remove_file(SYSTEMD_UNIT_PATH)?;
            println!("Removed {SYSTEMD_UNIT_PATH}");
        }

        run_command_ok("systemctl", &["daemon-reload"]);

        if Path::new(BINARY_INSTALL_PATH).exists() {
            std::fs::remove_file(BINARY_INSTALL_PATH)?;
            println!("Removed {BINARY_INSTALL_PATH}");
        }

        run_command_ok("userdel", &[SERVICE_USER]);
    }

    #[cfg(target_os = "macos")]
    {
        run_command_ok(
            "launchctl",
            &["bootout", &format!("system/{LAUNCHD_LABEL}")],
        );

        if Path::new(LAUNCHD_PLIST_PATH).exists() {
            std::fs::remove_file(LAUNCHD_PLIST_PATH)?;
            println!("Removed {LAUNCHD_PLIST_PATH}");
        }

        if Path::new(BINARY_INSTALL_PATH).exists() {
            std::fs::remove_file(BINARY_INSTALL_PATH)?;
            println!("Removed {BINARY_INSTALL_PATH}");
        }
    }

    println!("Service uninstalled. Config, log, and data directories are preserved.");
    Ok(())
}

pub fn validate(args: &ValidateArgs) -> Result<(), ServeError> {
    let config_path = match &args.config {
        Some(p) => p.clone(),
        None => discover_config_path()?,
    };

    if !config_path.exists() {
        return Err(ServeError::Service(format!(
            "Config file not found: {}",
            config_path.display()
        )));
    }

    println!("Validating: {}", config_path.display());

    let content = std::fs::read_to_string(&config_path)?;
    let config: ServeArgs = toml::from_str(&content)
        .map_err(|e| ServeError::Service(format!("Invalid config: {e}")))?;

    println!("Configuration is valid:");
    println!("  path: {}", config.get_path().display());
    println!("  addr: {}:{}", config.addr, config.port);
    println!("  compression: {}", !config.disable_compression);
    if !config.proxy.is_empty() {
        println!("  proxy routes: {}", config.proxy.len());
    }
    if let Some(ref log_path) = config.log_path {
        println!("  log_path: {}", log_path.display());
    }
    if let Some(ref not_found) = config.not_found {
        println!("  not_found: {}", not_found.display());
        if config.ok {
            println!("  ok: true (SPA mode)");
        }
    }
    if let Some(crate::config::Subcommands::Tls(ref tls)) = config.subcommand {
        println!(
            "  tls: cert={}, key={}",
            tls.cert.display(),
            tls.key.display()
        );
        if tls.redirect_http {
            println!("  redirect_http: true");
        }
    }

    Ok(())
}

pub fn restart() -> Result<(), ServeError> {
    require_root()?;

    #[cfg(target_os = "linux")]
    {
        run_command("systemctl", &["restart", "serve.service"])?;
    }

    #[cfg(target_os = "macos")]
    {
        run_command(
            "launchctl",
            &["kickstart", "-k", &format!("system/{LAUNCHD_LABEL}")],
        )?;
    }

    println!("Service restarted");
    Ok(())
}

pub fn reload() -> Result<(), ServeError> {
    require_root()?;

    // Validate config before restarting
    let config_path = discover_config_path()?;
    println!("Validating config: {}", config_path.display());

    let content = std::fs::read_to_string(&config_path)?;
    let _config: ServeArgs = toml::from_str(&content)
        .map_err(|e| ServeError::Service(format!("Invalid config, aborting reload: {e}")))?;

    #[cfg(target_os = "linux")]
    {
        run_command("systemctl", &["restart", "serve.service"])?;
    }

    #[cfg(target_os = "macos")]
    {
        run_command(
            "launchctl",
            &["kickstart", "-k", &format!("system/{LAUNCHD_LABEL}")],
        )?;
    }

    println!("Configuration reloaded");
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
pub fn status() -> Result<(), ServeError> {
    // Get installed binary version
    let version = if Path::new(BINARY_INSTALL_PATH).exists() {
        run_command(BINARY_INSTALL_PATH, &["--version"]).unwrap_or_else(|_| "unknown".to_string())
    } else {
        "not installed".to_string()
    };
    println!("Version: {version}");

    // Get config path
    match discover_config_path() {
        Ok(config_path) => println!("Config: {}", config_path.display()),
        Err(_) => println!("Config: not found (service not installed)"),
    }

    // Get service status
    #[cfg(target_os = "linux")]
    {
        let active = run_command_ok("systemctl", &["is-active", "serve.service"]);
        println!(
            "Status: {}",
            if active.is_empty() {
                "unknown"
            } else {
                &active
            }
        );
    }

    #[cfg(target_os = "macos")]
    {
        let output = run_command_ok("launchctl", &["print", &format!("system/{LAUNCHD_LABEL}")]);
        if output.is_empty() {
            println!("Status: not loaded");
        } else if output.contains("state = running") {
            println!("Status: running");
        } else {
            println!("Status: loaded (not running)");
        }
    }

    Ok(())
}
