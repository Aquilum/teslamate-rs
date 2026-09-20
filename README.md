# teslamate-rs

Single-process replacement for the TeslaMate stack (Elixir app + Postgres + Grafana + Mosquitto). Rust binary, SQLite, TeslaMate Grafana dashboards served in-process.

Named **teslamate-rs** so it does not collide with the legacy `teslamate` Docker project still running on this host (`:4000` / `teslamate-database-1`).

## Commands

```
teslamate-rs import --docker teslamate-database-1
teslamate-rs serve --bind 127.0.0.1 --port 4010
teslamate-rs login --refresh-token …
teslamate-rs doctor
```

Default database: `$TESLAMATE_RS_DB` or `$TESLAMATE_RS_HOME/teslamate-rs.sqlite` or `/srv/teslamate-rs/teslamate-rs.sqlite`.

Debian package (`dpkg-buildpackage -us -uc -b -d`) installs `/usr/bin/teslamate-rs`, a systemd unit bound to `127.0.0.1:4010`, and the data directory `/srv/teslamate-rs`.

`import` copies the live TeslaMate Postgres schema 1:1 via `docker exec … COPY`. Tokens are **not** imported; run `login` if you want the Owner API logger.

`serve` binds **4010** by default so Grafana/TeslaMate on 3000/4000 stay untouched. The logger is idle until `login` stores a refresh token.

## Build

```
cargo build --release --manifest-path /home/tdewey/src/teslamate-rs/Cargo.toml
```
