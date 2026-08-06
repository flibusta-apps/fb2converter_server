# Spec 02: Temp cleanup races active conversions; error paths leak dirs; panic on seek

- **Priority:** high
- **Effort:** M
- **Category:** correctness

## Problem(s)

### 02.1 Cleanup cron deletes in-flight conversion files (no age check)
`src/main.rs:27-49` deletes every `/tmp` entry whose name contains a `-`:

```rust
if !name.contains('-') { continue; }
...
if metadata.is_dir() { remove_dir_all(child.path()).await; }
else { remove_file(child.path()).await; }
```

Active conversions create `/tmp/{uuid}` (output dir, `main.rs:58`) and `/tmp/{uuid}.fb2` (`main.rs:65`) — both contain `-` (UUIDs are hyphenated). The cron has **no age/mtime check** (unlike the sibling service, which requires `> 3600s`). When the 6-hourly job (`main.rs:251`, cron `0 0 */6 * * *`) fires during a conversion, it deletes the output directory and input file of a request that is still running or still streaming its result, corrupting that response. It also indiscriminately deletes any unrelated hyphenated `/tmp` entry.

**Fix:** Only delete entries older than a threshold (e.g. `metadata.modified()`/`created()` elapsed `> N`), matching a specific prefix the service owns; skip entries younger than the max conversion timeout. Prefer per-request cleanup (see 02.2) as the primary mechanism and treat the cron as a backstop.

### 02.2 Error paths leak the output directory
`main.rs:59` creates `/tmp/{prefix}` up front. The only cleanup of that directory is the success path's `tokio::spawn(remove_dir_all(...))` at `main.rs:195-197`. Every early `return` after directory creation — tempfile creation failure (`main.rs:71`), open failure (`main.rs:79`), body read/write errors (`main.rs:89`, `main.rs:96`), invalid `file_format` (`main.rs:105`), `fb2c` non-zero/killed (`main.rs:128`, `main.rs:132`), missing output (`main.rs:171`), etc. — returns **without** removing the output dir. Every failed request leaks a directory until the 6-hourly cron.

**Fix:** Use a scope guard (or an RAII wrapper / `defer`-style helper) that removes the output dir on all exit paths, not just success.

### 02.3 `file_format` validated after the whole body is spooled to disk
`main.rs:103-106` validates the format only **after** the entire body has been streamed to a temp file (`main.rs:83-101`):

```rust
let allowed_formats = ["epub".to_string(), "mobi".to_string()];
if !allowed_formats.contains(&file_format.to_lowercase()) {
    return StatusCode::BAD_REQUEST.into_response();
}
```

A bogus `file_format` still forces a full upload to disk first (wasted IO/disk, and a leaked output dir per 02.2). Note: the value is validated before it reaches `fb2c`, so there is **no shell/argument injection** (`Command` is invoked without a shell) — but the ordering is wasteful. Also, validation lowercases the input while the original casing is passed to `fb2c` (`main.rs:111`), so `EPUB` passes the check but may be rejected by the converter.

**Fix:** Validate `file_format` (and normalize to lowercase) at the very top of the handler, before creating any temp files or reading the body.

### 02.4 `unwrap()` on seek can abort the whole process
`main.rs:184`:

```rust
let content_len = result_file.seek(SeekFrom::End(0)).await.unwrap();
```

An IO error here panics; with `panic = 'abort'` (`Cargo.toml:13`) the panic terminates the entire server, not just the request. Also, `main.rs:96-98` returns `StatusCode::NO_CONTENT` (204) on a temp-file write error, which is a misleading success-ish status for an internal failure.

**Fix:** Replace the `unwrap` with error handling that returns `500`; use `metadata().len()` instead of seeking. Return `500` (not `204`) for write failures.

## Acceptance criteria
- The cleanup cron never deletes files/dirs belonging to an in-progress conversion (age-gated and prefix-scoped); a test simulates an active conversion during a sweep.
- Every handler exit path removes its output directory (verified by a test that triggers an error mid-conversion and asserts `/tmp` is clean).
- `file_format` is validated before the body is read; invalid formats return `400` without writing to disk.
- No `unwrap` on IO in the response path; write failures return `500`.
