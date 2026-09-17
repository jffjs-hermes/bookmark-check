# bookmark-check

A CLI for checking links in Markdown files.

## Setup

Install the Rust toolchain with [rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Restart your shell after installation, or load Cargo's environment:

```sh
. "$HOME/.cargo/env"
```

## Build

```sh
cargo build
```

## Run

```sh
cargo run -- <file.md>
```

## Test

```sh
cargo test
```
