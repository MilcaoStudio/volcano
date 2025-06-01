# Build
FROM rust:slim-bookworm AS build
USER 0:0
WORKDIR /home/rust

RUN mkdir volcano
WORKDIR /home/rust/volcano
COPY Cargo.toml Cargo.lock ./

# Create lib
RUN USER=root cargo new --lib volcano-sfu
COPY volcano-sfu ./volcano-sfu

# Create server
RUN USER=root cargo new --bin server
COPY server ./server

# Build
RUN cargo build --locked --release
RUN cargo install --path server
RUN rm */src/*.rs target/release/deps/volcano*

# Bundle
FROM gcr.io/distroless/cc-debian12
COPY --from=build /usr/local/cargo/bin/server ./volcano-server
COPY config.toml ./config.toml

# Signaling server port
EXPOSE 4000/tcp

#TURN server port
EXPOSE 3478/udp

ENV RUST_LOG=debug

CMD ["./volcano-server"]