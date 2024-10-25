# Volcano

Rust implementation of a WebRTC Selective Forwarding Unit

A selective forwarding unit is a video routing service which allows webrtc sessions to scale more efficiently. 

## Build and run

### Build
```sh
cargo build
```

### Run server
```sh
RUST_LOG=debug
./target/debug/server
```

### License

MIT License - see [LICENSE](LICENSE) for full text