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
Options:
  -p, --port <PORT>            port to listen on [default: 3000]
  -a, --addr <ADDR>            address to listen on [default: 127.0.0.1]
  -l, --log-level <LOG_LEVEL>  log level [default: error] [possible values: error, warn, info, debug, trace]
      --disable-compression    compression layer is enabled by default
      --not-found <NOT_FOUND>  path to 404 page. By default, 404 is empty
      --ok                     override with 200 OK. Useful for SPA. Requires --not-found
  -h, --help                   Print help
  -V, --version                Print version
```

## Arguments

```
  [PATH]  path to the directory to serve. Defaults to the current directory
```

## Commands

```
  tls   Adds TLS support
  help  Print this message or the help of the given subcommand(s)
```

### tls

Adds TLS support

```
Usage: serve tls --cert <CERT> --key <KEY>

Options:
  -c, --cert <CERT>  path to the certificate file
  -k, --key <KEY>    path to the private key file
  -h, --help         Print help

```

#### Generate self signed certificate for localhost

```shell
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 \
  -nodes -keyout localhost.key -out localhost.crt -subj "/CN=localhost" \
  -addext "subjectAltName=IP:127.0.0.1"
```

## Minimum supported Rust version

serve MSRV is 1.70
