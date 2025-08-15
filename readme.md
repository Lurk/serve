# Serve

A simple tool to serve static files from a directory

## Installation

```shell
cargo install --git https://github.com/Lurk/serve serve
```

## Usage

```
serve [OPTIONS] [PATH] [COMMAND]
```

## Options:

```
  -p, --port <PORT>
          Port to listen on

          [default: 3000]

  -a, --addr <ADDR>
          Address to listen on

          [default: 127.0.0.1]

      --disable-compression
          Compression layer is enabled by default

      --not-found <NOT_FOUND>
          Path to 404 page. By default, 404 is empty

      --ok
          Override with 200 OK. Useful for SPA. Requires --not-found

  -v, --verbose...
          Increase logging verbosity

  -q, --quiet...
          Decrease logging verbosity

      --log-path <LOG_PATH>
          Path to the directory where logs will be stored. If not specified, logs will be printed to stdout.
          If specified, logs will be written to the file (log_path/serve.YYYY-MM-DD.log) and rotated daily.
          If the directory does not exist, it will be created.

      --log-max-files <LOG_MAX_FILES>
          Maximum number of log files to keep. Defaults to 7

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

```

## Arguments

```
  [PATH] Path to the directory to serve. Defaults to the current directory
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

## Generate self signed certificate for localhost

```shell
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 \
  -nodes -keyout localhost.key -out localhost.crt -subj "/CN=localhost" \
  -addext "subjectAltName=IP:127.0.0.1"
```

## Minimum supported Rust version

serve MSRV is 1.75
