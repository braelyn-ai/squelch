# squelch-tui

A deliberately minimal local operator's viewer — NOT the product surface. It opens the real SQLite store (same db the daemon uses), live-refreshes every 2s, and shows the ranked digest with the squelch line drawn in. It exists for setup, debugging, and rule tuning from a terminal on the box.

It is read-only toward mail. The single write it performs is sender-rule editing (the "squelch profile"). It is also the only surface that lists sealed messages at all — via a local-only store call, never over HTTP — and even here bodies stay hidden until explicitly revealed.

## Run

```sh
cargo run --bin squelch-tui
```

Reads `SQUELCH_DB_PATH` and `SQUELCH_ACCOUNT_EMAIL` (same resolution as every other binary).

## Keys

| Key | Action |
|---|---|
| `j` / `k` | move |
| `Enter` | drill into thread detail |
| `t` | edit sender rule for the selected sender |
| `T` | list all rules |
| `+` / `-` | adjust the in-session squelch threshold |
| `s` | toggle below-the-line items |
| `r` | reveal sealed subjects |
| `g` | refresh now |
| `q` | quit |
