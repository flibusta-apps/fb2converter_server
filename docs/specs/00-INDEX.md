# fb2converter_server — Audit Spec Index

Audit of `fb2converter_server` (~315 lines Rust). Findings are grouped into spec files; each is self-contained with file:line evidence and acceptance criteria.

| Spec | Title | Priority | Effort | Category |
|------|-------|----------|--------|----------|
| [01](01-hostile-input-and-resource-limits.md) | Unbounded input, no concurrency limit, no fb2c timeout | high | M | reliability |
| [02](02-temp-cleanup-races-and-error-paths.md) | Temp cleanup races active conversions; error paths leak dirs; panic on seek | high | M | correctness |
| [03](03-delivery-observability-tests.md) | Delivery, observability, and test gaps | medium | M | delivery |
| [04](04-performance-io.md) | I/O efficiency — buffering, syscall reduction, build hygiene | low | S | performance |

## Top risks
1. **Cleanup cron destroys in-flight work (02.1):** the 6-hourly sweep deletes every hyphenated `/tmp` entry with no age check, so it wipes the input/output of conversions that are still running or streaming.
2. **No fb2c timeout / no concurrency limit / unbounded body (01):** a hung or hostile FB2, or a burst of requests, exhausts CPU/RAM/disk with no backpressure; `fb2c` also runs as root on untrusted XML.
3. **Process-killing panics and leaked temp dirs (02.2/02.4):** `seek(...).unwrap()` aborts the whole process (`panic = 'abort'`), and every error path leaks its `/tmp/{uuid}` output directory.
