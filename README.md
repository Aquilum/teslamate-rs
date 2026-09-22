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

Debian package (`dpkg-buildpackage -us -uc -b -d`) installs `/usr/bin/teslamate-rs`, a systemd unit bound to `127.0.0.1:4010`, `/etc/pam.d/teslamate-rs`, and the data directory `/srv/teslamate-rs`.

`import` copies the live TeslaMate Postgres schema 1:1 via `docker exec … COPY`. Tokens are **not** imported; run `login` if you want the Owner API logger.

`serve` binds **4010** by default so Grafana/TeslaMate on 3000/4000 stay untouched. The logger is idle until `login` stores a refresh token.

## Web UI accounts

`teslamate-rs serve` requires a signed-in profile. On a Linux server the Debian default is **PAM**: sign in with a Unix account in group `teslamate-rs`. On macOS (and Linux with `--auth local`) the first visitor creates the **admin** account (username + password, or a passkey); later users need an invite.

```bash
sudo adduser alice teslamate-rs          # allow alice to sign in (PAM)
sudo systemctl enable --now teslamate-rs
```

- First successful PAM login becomes **admin**. Later Linux users must be in group `teslamate-rs`.
- Passkeys still work after that account exists. Invites are disabled under PAM; Unix group membership is the invite.
- `root` cannot sign in unless `TESLAMATE_RS_PAM_ALLOW_ROOT=1`.
- `TESLAMATE_RS_AUTH=local` or `--auth local` restores the SQLite password database.

On a **Linux host**, keep teslamate-rs on loopback and put **nginx TLS** in front so session cookies are `Secure` and passkeys match the public hostname. See `docs/nginx-teslamate-rs.conf`. Pin origin with `TESLAMATE_RS_WEBAUTHN_ORIGIN` / `TESLAMATE_RS_WEBAUTHN_RP_ID` if needed.

Local development (no nginx):

```bash
cargo run --release -- serve --bind 127.0.0.1 --port 4011 --no-logger --auth local
# open http://localhost:4011  (use localhost, not 127.0.0.1, if you want passkeys)
```

Car telemetry stays in the shared SQLite file. Profiles only control who can open the dashboards.

Each account picks an interface, stored on that profile: **Classic dashboards** (the Grafana pages) or **Grouped** (Vehicle, Battery, Trips, Software). Grouped keeps each history on one page and adds the live vehicle card TeslaMate showed outside Grafana — locks, sentry, closures, tires, climate, and route — from the last Owner API poll.

## Build

```
cargo build --release --manifest-path /home/tdewey/src/teslamate-rs/Cargo.toml
```
