# fb2converter_server

HTTP API that converts FB2 files to `epub`/`mobi` via the external [`fb2c`](https://github.com/rupor-github/fb2converter) binary.

## Routes

- `POST /{file_format}` — convert an FB2 file.
  - `file_format`: `epub` or `mobi`.
  - Request body: raw FB2 bytes.
  - Requires `Authorization` header equal to `API_KEY`.
- `GET /health` — health check (used by the container `HEALTHCHECK`).
- `GET /metrics` — Prometheus metrics.

## Environment variables

### Required

- `API_KEY` — shared secret checked against the `Authorization` header on conversion requests.
- `SENTRY_DSN` — Sentry DSN for error reporting.

### Optional (with defaults)

- `MAX_BODY_BYTES` (default: `50MB`) — max accepted upload size.
- `FB2C_TIMEOUT_SECS` (default: `120`) — kills the `fb2c` subprocess past this many seconds.
- `MAX_CONCURRENT_CONVERSIONS` (default: `4`) — bounds concurrent `fb2c` processes; excess requests get `503`.
- `MAX_OUTPUT_BYTES` (default: `200MB`) — cap on converted file size.
- `CLEANUP_MAX_AGE_SECS` (default: `3600`) — the `/tmp` cleanup sweep only removes entries older than this.

## The `fb2c` dependency

- Pulled from [rupor-github/fb2converter](https://github.com/rupor-github/fb2converter) GitHub releases.
- Pinned to a specific version and verified by SHA-256 checksum in `docker/build.dockerfile`.
- When upgrading, update both the version and the checksum together.

## Trust boundary

Input is expected to arrive only from an internal `books_downloader` service, gated by the `API_KEY` header. This service does not perform its own untrusted-input sanitization beyond what `fb2c` provides — do not expose it directly to the public internet.
