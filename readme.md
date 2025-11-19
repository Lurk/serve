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
  tls   Adds TLS support
  help  Print this message or the help of the given subcommand(s)
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

[subcommand.Tls]
cert = "/var/certs/localhost.crt"
key = "/var/certs/localhost.key"
redirect_http = true
```



## Generate self signed certificate for localhost

```shell
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 \
  -nodes -keyout localhost.key -out localhost.crt -subj "/CN=localhost" \
  -addext "subjectAltName=IP:127.0.0.1"
```

## Minimum supported Rust version

serve MSRV is 1.82
