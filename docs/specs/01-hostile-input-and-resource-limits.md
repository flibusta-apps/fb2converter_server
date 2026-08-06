# Spec 01: Unbounded input, no concurrency limit, no fb2c timeout

- **Priority:** high
- **Effort:** M
- **Category:** reliability

## Problem(s)

### 01.1 Request body written to disk with no size limit
`src/main.rs:83-99` streams the entire request body to a temp file with no `DefaultBodyLimit` and no byte cap:

```rust
let mut data_stream = body.into_data_stream();
while let Some(chunk) = data_stream.next().await {
    ...
    tempfile_rw.write(data.as_ref()).await
}
```

`get_router()` (`main.rs:224-246`) never applies `DefaultBodyLimit`, so axum's default (2 MB) is... actually removed? No — axum's default 2 MB applies only to buffered extractors; `Body` streamed directly is unbounded. A caller with the API key can stream an arbitrarily large FB2 and fill `/tmp` (which is also where output and the cleanup cron operate), a disk-exhaustion DoS affecting every concurrent conversion.

**Fix:** Apply an explicit `DefaultBodyLimit::max(N)` sized to the largest legitimate FB2, and/or enforce a running byte counter that aborts the write past a threshold and returns `413 Payload Too Large`.

### 01.2 No timeout on the `fb2c` child process
`src/main.rs:108-116` runs the converter and awaits `.status()` with no timeout:

```rust
Command::new("/app/bin/fb2c").arg("convert")...
    .status().await
```

A crafted or pathological FB2 that makes `fb2c` (or its bundled `kindlegen`) hang or spin leaves the child running indefinitely, holding a request task, a temp file, and an output dir. Enough hung requests exhaust tasks/CPU/disk.

**Fix:** Wrap the child in `tokio::time::timeout`; on expiry, kill the child (`child.kill().await`) and return `504`/`500`. Use `Command::spawn` + `kill_on_drop(true)` so an aborted request also reaps the process.

### 01.3 No concurrency limit on conversions
Each `POST /{file_format}` spawns an independent `fb2c` process (`main.rs:108`). Nothing bounds how many run at once, so N concurrent requests fork N native converters, each spawning `kindlegen` for mobi — trivially CPU/RAM/disk exhaustion.

**Fix:** Gate conversions behind a bounded `tokio::sync::Semaphore` (or a `tower` concurrency/load-shed layer) sized to CPU count; return `503` when saturated.

### 01.4 Hostile FB2 handling relies entirely on fb2c
The service passes the uploaded bytes straight to `fb2c` (`main.rs:112`). FB2 is XML; the converter may be vulnerable to XML entity expansion / XXE / zip-bomb-style expansion into `/tmp`. There is no output-size cap on the produced epub/mobi before it is streamed back (`main.rs:184` reads whatever size `fb2c` produced).

**Fix:** Run `fb2c` with least privilege (see Spec 03) and cap output size; document the trust boundary (input arrives only from the internal `books_downloader`, guarded by API key).

## Acceptance criteria
- Requests exceeding a configured body-size limit are rejected with `413` before filling the disk.
- A conversion that runs longer than a configured timeout is killed and returns an error; no orphaned `fb2c` processes remain.
- Concurrent conversions are bounded; excess requests get `503` rather than exhausting the host.
