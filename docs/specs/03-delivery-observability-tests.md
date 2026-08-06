# Spec 03: Delivery, observability, and test gaps

- **Priority:** medium
- **Effort:** M
- **Category:** delivery

## Problem(s)

### 03.1 Container runs as root; no HEALTHCHECK; unverified downloaded binary
`docker/build.dockerfile` never creates or switches to a non-root user, so both the server and every spawned `fb2c`/`kindlegen` child run as `root` — an attacker who finds an RCE in `fb2c` on hostile FB2 (Spec 01.4) gets root in the container. There is no `HEALTHCHECK` despite a `/health` route (`main.rs:235`). The converter is fetched with `ADD https://github.com/.../fb2c-linux-amd64.zip` (`build.dockerfile:9`) with **no checksum verification**, so a compromised/altered release is silently trusted.

**Fix:** Add a non-root user and `USER` directive; run `fb2c` as that user. Add a `HEALTHCHECK`. Pin and verify the `fb2c` archive by SHA-256 (download, `sha256sum -c`, then unzip). Consider dropping Linux capabilities / read-only rootfs with a writable `/tmp` mount.

### 03.2 No `.dockerignore`; whole tree copied into build
`build.dockerfile:17` does `COPY . .` and there is no `.dockerignore`, so `target/`, `.git/`, docs, and any local `.env` are pulled into the builder layer, bloating the image and risking secret leakage.

**Fix:** Add a `.dockerignore` excluding `target/`, `.git/`, `docs/`, `*.env`, `.DS_Store`.

### 03.3 CI clippy non-blocking; no tests
`.github/workflows/rust-clippy.yml:44-49` runs clippy with `continue-on-error: true`; there is no `cargo test` job. There are zero tests. Conversion happy-path, format validation, cleanup, and error handling are unverified.

**Fix:** Add a blocking `cargo test` + `cargo clippy -- -D warnings` job. Add tests for `file_format` validation, `remove_temp_files` age/prefix gating (Spec 02.1), and the auth middleware.

### 03.4 Thin observability; no graceful shutdown; per-request env read
Only default `axum-prometheus` HTTP metrics are exported (`main.rs:225`); there are no counters for conversion success/failure, duration, or bytes. Sentry receives only `ERROR` events (`main.rs:302-305`). `axum::serve` has no graceful shutdown (`main.rs:281`), so SIGTERM cuts in-flight conversions and leaves temp files. `API_KEY` is read from the environment via `std::env::var` on **every** request (`main.rs:215`) and panics if unset (`main.rs:216`) — a per-request syscall and a panic-in-request-path hazard.

**Fix:** Read/validate `API_KEY` once at startup (e.g. `once_cell`/`LazyLock`, as the sibling service does). Add conversion metrics (outcome, format, duration). Add `with_graceful_shutdown` handling SIGTERM. Add a request-id layer.

### 03.5 Minor: duplicated auth, empty output validation, no README
The auth middleware (`main.rs:202-222`) is duplicated near-verbatim from `telegram_files_server`; consider a shared crate. There is no README documenting env vars (`API_KEY`, `SENTRY_DSN`), routes, or the `fb2c` dependency.

**Fix:** Add a README; optionally factor out shared auth.

## Acceptance criteria
- Production image runs as non-root, defines a `HEALTHCHECK`, and verifies the `fb2c` archive checksum.
- `.dockerignore` present; `target/`/`.git`/env files excluded from build context.
- CI fails on clippy warnings and failing tests; format-validation and cleanup logic have unit tests.
- `API_KEY` is loaded once at startup; SIGTERM triggers graceful shutdown; conversion outcome metrics are exported.
