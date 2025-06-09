# Volcano

Rust implementation of a WebRTC Selective Forwarding Unit

A selective forwarding unit is a video routing service which allows webrtc sessions to scale more efficiently. 

## Development

### Setup

Clone the repository.
```sh
git clone https://github.com/MilcaoStudio/volcano.git
cd volcano
```

Copy `config.example.toml` to `config.toml`.
```sh
cp config.example.toml config.toml
```

### Build and run

It's recommended to enable logging for `server` and `volcano_sfu`.
Set `RUST_LOG="debug"` to enable debug level for all crates (including `webrtc`).
```sh
RUST_LOG="server, volcano_sfu"
cargo run
```

### License

MIT License - see [LICENSE](LICENSE) for full text