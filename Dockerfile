FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY policies policies
COPY src src
RUN cargo build --release --locked && mkdir -p /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/tocsin /usr/local/bin/tocsin
COPY --from=build --chown=65532:65532 /out/data /data
WORKDIR /data
EXPOSE 4318
ENTRYPOINT ["tocsin"]
CMD ["serve", "--listen", "0.0.0.0:4318"]
