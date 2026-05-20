# Serve

A simple tool to serve static files from a directory

## Installation

```shell
cargo install --git https://github.com/Lurk/serve serve
```

## Usage

```
serve [OPTIONS] [COMMAND]
```

## Options:

```
  -c, --config <CONFIG>                Path to the configuration file.
                                       Command line arguments override the configuration file.
                                       If configuration file does not exist, it will be created with the current
                                       command line arguments.

                                       Supported format is TOML.

      --path <PATH>                    Path to the directory to serve. Defaults to the current directory
  -p, --port <PORT>                    Port to listen on [default: 3000]
  -a, --addr <ADDR>                    Address to listen on [default: 127.0.0.1]
      --disable-compression            Compression layer is enabled by default
      --not-found <NOT_FOUND>          Path to 404 page. By default, 404 is empty
      --ok                             Override with 200 OK. Useful for SPA. Requires --not-found
      --proxy <PROXY>                  Proxy route in the format /path=http://host:port.
                                       Can be specified multiple times. HTTPS upstreams are not supported.
  -v, --verbose...                     Increase logging verbosity
  -q, --quiet...                       Decrease logging verbosity
      --log-path <LOG_PATH>            Path to the directory where logs will be stored.
                                       If not specified, logs will be printed to stdout.
                                       If specified, logs will be written to the file: log_path/serve.YYYY-MM-DD.log
                                       and rotated daily.
                                       If the directory does not exist, it will be created.

      --log-max-files <LOG_MAX_FILES>  Maximum number of log files to keep [default: 7]
  -h, --help                           Print help
  -V, --version                        Print version
```

## Commands

```
  tls        Adds TLS support
  init       Create a default configuration file
  install    Install serve as a system service
  uninstall  Remove the serve system service
  validate   Validate serve configuration
  restart    Restart the system service (e.g. after binary update)
  reload     Reload the service configuration
  status     Show service status, version, and config path
  help       Print this message or the help of the given subcommand(s)
```

### tls

Adds TLS support

```
Usage: serve [OPTIONS] [PATH] tls --cert <CERT> --key <KEY> --redirect-http

Options:
  -c, --cert <CERT>    path to the certificate file
  -k, --key <KEY>      path to the private key file
      --redirect-http  Redirect HTTP to HTTPS. Works only if 443 port is used
```

### proxy

Reverse proxy requests matching a path prefix to an upstream HTTP server.

```
serve --proxy /api=http://localhost:8080
```

Multiple proxies can be specified:

```
serve --proxy /api=http://localhost:8080 --proxy /ws=http://localhost:9090
```

By default, the matching prefix is stripped before forwarding (e.g., `/api/users` is forwarded as `/users`).
To disable prefix stripping, use the config file with `strip_prefix = false`.
The proxy sets `x-forwarded-for` and `x-forwarded-proto` headers on forwarded requests.
HTTPS upstreams are not supported.

## Stats (optional)

`serve` ships with an opt-in HTTP analytics subsystem that records request
counts and bytes served, broken down by URL path and HTTP status class. A
small password-gated HTML dashboard lives at `/__stats__`.

Stats are **off by default**. To enable, add a `[stats]` table to your
`serve.toml`:

```toml
[stats]
db_path = "/var/lib/serve/stats.db"   # SQLite file; parent dir must be writable
session_ttl_days = 30                  # default
# secure_cookies = true                # force the Secure attribute on auth cookies.
                                       # Omit to follow the TLS subcommand; set to
                                       # true when TLS terminates upstream (proxy).
# url_prefix = "/admin/stats"          # default: "/__stats__". Must start with /,
                                       # must not end with /, and must not contain
                                       # "//" or "..".
```

On the very first start with `[stats]` configured, `serve` prints a setup
token to **stderr** only — the token is intentionally never written through
`tracing` so `--log-path` won't persist it to a log file on disk:

```
stats setup token: 8f4c…
```

Visit `https://your-host/__stats__/` in a browser. You will be redirected to
`/__stats__/setup`, where you enter the token plus a password of your choice.
The password is hashed with argon2id and stored in the SQLite file; the token
is consumed and cannot be reused.

After setup, log in at `/__stats__/login` to see:

- Time-series of requests/bytes broken down by 2xx/3xx/4xx/5xx
- Top 30 paths (toggle by requests or bytes)
- Windows: 1 day, 7 days, 30 days, 12 months

### Retention

Minute buckets are kept for 25 hours, hourly buckets for 32 days, daily
buckets for 400 days. Older rows are pruned automatically.

### Resetting the password

Stop `serve`, delete the SQLite file at `db_path`, and restart. A new setup
token will be printed.

### Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| `/__stats__` returns 404 | `[stats]` not in config; section missing. |
| Setup form keeps rejecting token | Token only printed on first start with empty DB. Delete DB to regenerate. |
| Dashboard shows zero traffic | Writer flushes every 10s; very recent requests may not yet be visible. |
| `serve` refuses to start with "stats db_path parent missing" | Create the parent directory or fix the path. |

## Config file:


### Example config file:

```toml
# Configuration for Serve (https://github.com/Lurk/serve)

path = "/var/www"
port = 3000
addr = "127.0.0.1"
disable_compression = false
ok = false
log_level = "trace" # off, error, warn, info, debug, trace
log_path = "/var/log/serve"
log_max_files = 7

[[proxy]]
path = "/api"
upstream = "http://localhost:8080"
strip_prefix = true

[subcommand.Tls]
cert = "/var/certs/localhost.crt"
key = "/var/certs/localhost.key"
redirect_http = true
```



## Service Management

Serve can install itself as a system service on Linux (systemd) and macOS (launchd).

### Create a config file

```shell
serve init
```

Creates a default config at the platform-specific path (`/etc/serve/serve.toml` on Linux, `/Library/Application Support/serve/serve.toml` on macOS). To specify a custom path:

```shell
serve init --path /path/to/serve.toml
```

### Install as a service

```shell
sudo serve install --config /etc/serve/serve.toml
```

This validates the config, copies the binary to `/usr/local/bin/serve`, creates log and data directories, and registers the system service. On Linux it also creates a dedicated `serve` user.

### Validate configuration

```shell
serve validate
```

Auto-detects the config path from the installed service. To validate a specific file:

```shell
serve validate --config /path/to/serve.toml
```

### Restart (after binary update)

```shell
sudo serve restart
```

### Reload (after config change)

```shell
sudo serve reload
```

Validates the config before restarting. Aborts if the config is invalid.

### Check status

```shell
serve status
```

Shows running status, installed version, and config path.

### Uninstall

```shell
sudo serve uninstall
```

Removes the binary, service definition, and service user. Config, log, and data directories are preserved.

### Firewall

If binding to ports 443/80, ensure they are open in both the OS firewall and any cloud firewall (e.g., Hetzner Cloud Firewall).

## Generate self signed certificate for localhost

```shell
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 \
  -nodes -keyout localhost.key -out localhost.crt -subj "/CN=localhost" \
  -addext "subjectAltName=IP:127.0.0.1"
```

## Minimum supported Rust version

serve MSRV is 1.89
