# Spec 04: I/O efficiency — buffering, syscall reduction, build hygiene

- **Priority:** low
- **Effort:** S
- **Category:** performance

Spec 01 owns the load-bearing resource controls (input size limit, fb2c timeout, concurrency limit); Spec 02 owns temp-file lifecycle; Spec 03 owns delivery. This spec is the remaining I/O polish — individually small, worth one batched PR.

## Problem(s)

### 04.1 Upload loop writes each network chunk straight to disk

`src/main.rs:83-101`: `while let Some(chunk) = stream.next()` → `write()` per chunk issues one syscall per network chunk (typically 8–64 KB). For a 10 MB FB2 that is hundreds of small writes.

**Fix:** Wrap the target file in `tokio::io::BufWriter::with_capacity(256 * 1024, file)`; `flush()` (already present at line 101) before spawning fb2c.

### 04.2 File size read via double seek

`src/main.rs:183-185`: `seek(End(0))` + `seek(Start(0))` — two syscalls and an `unwrap` (Spec 02's concern) to learn the length.

**Fix:** `result_file.metadata().await?.len()` — one syscall, no seek state to restore.

### 04.3 Result streaming uses default chunk size

`src/main.rs:187`: `ReaderStream::new(result_file)` reads in small default chunks. **Fix:** `ReaderStream::with_capacity(result_file, 64 * 1024)`.

### 04.4 Output file located by directory scan

`src/main.rs:139-165`: after fb2c finishes, the code `read_dir`s the output directory and linearly searches for a file with the requested extension. Works, but is an extra directory walk and fails silently in odd ways if fb2c emits auxiliary files.

**Fix:** fb2c's output name is deterministic from the input name — derive the expected path (`/tmp/{uuid}/{uuid}.{fmt}`) and `File::open` it directly; keep the scan only as a logged fallback. Verify the naming rule against the pinned fb2c version first.

### 04.5 Per-request constants and dead cargo features

- `src/main.rs:103-104`: `allowed_formats` rebuilt as `Vec<String>` per request → `const ALLOWED_FORMATS: [&str; 2] = ["epub", "mobi"];` and compare as `&str`.
- `src/main.rs:56-65`: UUID formatted into 3 separate `format!` strings — build the paths once with `PathBuf`.
- `Cargo.toml`: `tokio = ["full"]` → enumerate; axum `multipart` feature appears unused (body streaming is used instead) — remove if `cargo check` agrees.
- `docker/`: no `.dockerignore`, so `COPY . .` ships `target/`, `.git/` into the build context — add one (also shrinks build times).

## Acceptance criteria

- Upload path performs buffered writes (≤ payload/256KB write syscalls, observable via `strace`/dtruss spot check or just code inspection).
- No `seek`-based length calculation; no directory scan on the happy path (fallback logged when the expected name is missing).
- Build green with trimmed features; docker build context excludes `target/` and `.git/`.
