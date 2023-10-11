# Serve

A simple tool to serve static files from a directory

## Installation

```shell
cargo install --git https://github.com/Lurk/serve serve
```

## Usage

To serve files from current folder:

```shell
serve
```

To serve files from specific folder

```shell
serve /path/to/specific/folder
```

## Options:

```
  -p, --port <PORT>            port to listen on [default: 3000]
  -l, --log-level <LOG_LEVEL>  log level [default: error] [possible values: error, warn, info, debug, trace]
  -h, --help                   Print help
  -V, --version                Print version
```

## Minimum supported Rust version

serve MSRV is 1.70
