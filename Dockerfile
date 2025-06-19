# Build
FROM rust:slim-bookworm AS build
USER 0:0
WORKDIR /home/rust

RUN mkdir volcano
WORKDIR /home/rust/volcano
COPY Cargo.toml Cargo.lock ./
COPY volcano-sfu ./volcano-sfu
COPY server ./server

# Build
RUN cargo install --locked --path server --root /usr/local

# Bundle
FROM gcr.io/distroless/cc-debian12
COPY --from=build /usr/local/bin/server /etc/volcano/volcano-server
COPY config.example.toml /etc/volcano/config.toml

# Signaling server port
EXPOSE 4000/tcp

# TURN server port
#EXPOSE 3478/udp

ENV RUST_LOG=debug

CMD ["./etc/volcano/volcano-server"]