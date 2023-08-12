FROM debian:bullseye-slim AS convert_downloader

RUN apt-get update \
    && apt-get install --no-install-recommends -y unzip \
    && rm -rf /var/lib/apt/lists/*

# Get converter bin
WORKDIR  /root/fb2converter
ADD https://github.com/rupor-github/fb2converter/releases/download/v1.71.0/fb2c_linux_amd64.zip ./
RUN unzip fb2c_linux_amd64.zip


FROM rust:bullseye AS builder

WORKDIR /app

COPY . .

RUN cargo build --release --bin fb2converter_server


FROM debian:bullseye-slim

RUN apt-get update \
    && apt-get install -y openssl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN update-ca-certificates

WORKDIR /app

COPY --from=convert_downloader /root/fb2converter/kindlegen /app/bin/
COPY --from=convert_downloader /root/fb2converter/fb2c /app/bin/

COPY --from=builder /app/target/release/fb2converter_server /usr/local/bin
ENTRYPOINT ["/usr/local/bin/fb2converter_server"]
