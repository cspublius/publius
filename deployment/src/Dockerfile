# Leveraging the pre-built Docker images with 
# cargo-chef and the Rust toolchain
FROM rust:slim AS chef
WORKDIR /app
RUN cargo install cargo-chef
RUN apt-get update
RUN apt-get install make

FROM chef AS planner
WORKDIR /app
COPY . .
COPY ./.obelisk_build/Cargo.toml ./Cargo.toml
COPY ./.obelisk_build/main.rs ./src/main.rs
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder 
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
RUN cargo chef cook --release --recipe-path recipe.json
# Build application
COPY . .
COPY ./.obelisk_build/Cargo.toml ./Cargo.toml
COPY ./.obelisk_build/main.rs ./src/main.rs
RUN cat src/main.rs
RUN cargo build --release

# We do not need the Rust toolchain to run the binary!
FROM python:slim AS runtime
WORKDIR /app
COPY scripts scripts
RUN pip3 install -r scripts/requirements.txt

COPY --from=builder /app/target/release/obelisk_main .
ENTRYPOINT ["./obelisk_main"]
