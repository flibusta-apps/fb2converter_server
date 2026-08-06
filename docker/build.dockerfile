FROM debian:bookworm-slim AS convert_downloader

RUN apt-get update \
    && apt-get install --no-install-recommends -y unzip curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Get converter bin
WORKDIR  /root/fb2converter
# NOTE: version and checksum must be updated together when upgrading fb2c.
# Release: v1.75.4 - https://github.com/rupor-github/fb2converter/releases/tag/v1.75.4
ADD https://github.com/rupor-github/fb2converter/releases/download/v1.75.4/fb2c-linux-amd64.zip ./
RUN echo "c8d42083754052cf0b8502b17b2291f591e55aa665ec15ccf4df119eb71821b9  fb2c-linux-amd64.zip" | sha256sum -c -
RUN unzip fb2c-linux-amd64.zip


FROM rust:bookworm AS builder

WORKDIR /app

COPY . .

RUN cargo build --release --bin fb2converter_server


FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y openssl ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN update-ca-certificates

RUN groupadd -r app && useradd -r -g app -d /app -s /usr/sbin/nologin app

WORKDIR /app
RUN chown app:app /app

COPY ./scripts/*.sh /
RUN chmod +x /*.sh

COPY --from=convert_downloader --chown=app:app /root/fb2converter/kindlegen /app/bin/
COPY --from=convert_downloader --chown=app:app /root/fb2converter/fb2c /app/bin/

COPY --from=builder /app/target/release/fb2converter_server /usr/local/bin

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

USER app

CMD ["/start.sh"]
