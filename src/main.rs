mod app_state;
mod audit;
mod auth;
mod auth_http;
mod db;
mod detail;
mod docker_util;
mod import;
mod invoices;
mod live;
mod logger;
mod mock;
mod pam_auth;
mod query_allowlist;
mod refresh;
mod seed;
mod server;
mod sql;
mod tesla;
mod token_crypto;
mod tokens;
mod users;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "teslamate-rs",
    version,
    about = "Single-process TeslaMate (Rust + SQLite)"
)]
struct Cli {
    /// SQLite database path
    #[arg(long, env = "TESLAMATE_RS_DB", global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import a live TeslaMate Postgres instance via docker exec
    Import {
        #[arg(long, default_value = "teslamate-database-1")]
        docker: String,
        #[arg(long, default_value = "teslamate")]
        user: String,
        #[arg(long, default_value = "teslamate")]
        dbname: String,
    },
    /// Import Cloak-encrypted Owner API tokens from a live TeslaMate instance
    ImportTokens {
        #[arg(long, default_value = "teslamate-database-1")]
        docker: String,
        #[arg(long, default_value = "teslamate-teslamate-1")]
        app: String,
        #[arg(long, default_value = "teslamate")]
        user: String,
        #[arg(long, default_value = "teslamate")]
        dbname: String,
    },
    /// Load synthetic drives/charges/positions (no Tesla account)
    Seed,
    /// Serve dashboards (and start the Owner API logger if tokens exist)
    Serve {
        #[arg(long, env = "TESLAMATE_RS_BIND", default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, env = "TESLAMATE_RS_PORT", default_value_t = 4010)]
        port: u16,
        #[arg(long, env = "TESLAMATE_RS_NO_LOGGER")]
        no_logger: bool,
        /// Password backend: `local` (SQLite argon2) or `pam` (Linux). Default: PAM when
        /// `/etc/pam.d/teslamate-rs` exists, otherwise local. Also `TESLAMATE_RS_AUTH`.
        #[arg(long, env = "TESLAMATE_RS_AUTH")]
        auth: Option<String>,
    },
    /// Pull Tesla Supercharger invoices (account API; does not wake the car)
    SyncInvoices {
        /// Force OAuth refresh, list vehicles, and walk every history page
        #[arg(long)]
        full: bool,
    },
    /// TeslaMate-style Owner API read sweep (no wake_up; vehicle_data only if already online)
    Refresh,
    /// Store an Owner API refresh token and register vehicles
    Login {
        #[arg(long)]
        refresh_token: String,
    },
    /// Hit Tesla auth + products + vehicle_data (no token values printed)
    Probe,
    /// Seed mock history, run a local Tesla API mock, login, and serve dashboards
    Demo {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 4010)]
        port: u16,
        #[arg(long, default_value_t = 4070)]
        mock_port: u16,
    },
    /// Schema + row counts
    Doctor,
}

fn point_at_mock(bind: &str, mock_port: u16) {
    let base = format!("http://{bind}:{mock_port}");
    std::env::set_var("TESLA_API_HOST", &base);
    std::env::set_var("TESLA_AUTH_URL", format!("{base}/oauth2/v3/token"));
    std::env::set_var(
        "TESLA_WSS_HOST",
        format!("ws://{bind}:{mock_port}/streaming/"),
    );
    std::env::set_var("TESLAMATE_RS_POLL_SECS", "1");
    std::env::set_var("TESLAMATE_RS_ALLOW_INSECURE_TESLA", "1");
}

async fn probe(db: &db::Db) -> Result<()> {
    let stored = logger::load_tokens(db)?
        .ok_or_else(|| anyhow::anyhow!("no tokens; run import-tokens or login"))?;
    let tesla = tesla::Tesla::from_refresh_token(&stored.refresh_token).await?;
    logger::store_tokens(db, tesla.tokens())?;
    let mut tesla = tesla;
    let products = tesla.products().await?;
    println!("products: {}", products.len());
    for p in &products {
        let name = tesla::str_field(p, &["display_name"]).unwrap_or("?");
        let state = tesla::str_field(p, &["state"]).unwrap_or("?");
        let id = tesla::i64_field(p, &["id"]).unwrap_or(0);
        println!("  {name} id={id} state={state}");
        if id != 0 && state == "online" {
            let data = tesla.vehicle_data(id).await?;
            let soc = tesla::i64_field(&data, &["charge_state", "battery_level"]);
            let shift = tesla::str_field(&data, &["drive_state", "shift_state"]).unwrap_or("-");
            println!("  vehicle_data soc={soc:?}% shift={shift}");
        } else if id != 0 {
            println!("  skipped vehicle_data (car is {state}; TeslaMate does not wake it)");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "teslamate_rs=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(db::default_db_path);
    let db = db::open(&db_path)?;

    match cli.cmd {
        Cmd::Import {
            docker,
            user,
            dbname,
        } => {
            let mut conn = db.lock();
            import::import_from_docker(
                &mut conn,
                &import::ImportOpts {
                    docker,
                    user,
                    dbname,
                },
            )?;
            eprint!("{}", import::summarize(&conn)?);
        }
        Cmd::ImportTokens {
            docker,
            app,
            user,
            dbname,
        } => {
            tokens::import_tokens(&db, &docker, &app, &user, &dbname)?;
            println!("tokens imported into {}", db_path.display());
            println!("refreshing against Tesla Owner API …");
            probe(&db).await?;
        }
        Cmd::Seed => {
            let mut conn = db.lock();
            seed::seed(&mut conn)?;
            print!("{}", import::summarize(&conn)?);
        }
        Cmd::Serve {
            bind,
            port,
            no_logger,
            auth,
        } => {
            if !no_logger {
                let db2 = db.clone();
                tokio::spawn(async move { logger::run(db2).await });
            }
            if invoices::enabled() {
                let db3 = db.clone();
                tokio::spawn(async move { invoices::run(db3).await });
            }
            let addr: SocketAddr = format!("{bind}:{port}").parse()?;
            let backend = auth::resolve_password_backend(auth.as_deref())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            server::serve(db, addr, backend).await?;
        }
        Cmd::SyncInvoices { full } => {
            let stats = invoices::sync_once_opts(&db, full).await?;
            println!(
                "charging invoices pages={} fetched={} stored={} matched={} cost_updated={}",
                stats.pages, stats.fetched, stats.stored, stats.matched, stats.cost_updated
            );
        }
        Cmd::Refresh => {
            for line in refresh::run(&db).await? {
                println!("{line}");
            }
        }
        Cmd::Login { refresh_token } => {
            logger::login(&db, &refresh_token).await?;
            println!("tokens stored in {}", db_path.display());
        }
        Cmd::Probe => {
            probe(&db).await?;
        }
        Cmd::Demo {
            bind,
            port,
            mock_port,
        } => {
            {
                let mut conn = db.lock();
                seed::seed(&mut conn)?;
                print!("seeded\n{}", import::summarize(&conn)?);
            }
            point_at_mock(&bind, mock_port);
            let mock_addr: SocketAddr = format!("{bind}:{mock_port}").parse()?;
            tokio::spawn(async move {
                if let Err(e) = mock::serve(mock_addr).await {
                    tracing::error!("mock api: {e:#}");
                }
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            logger::login(&db, "mock-refresh-token").await?;
            println!("mock Owner API: http://{bind}:{mock_port}");
            probe(&db).await?;
            let db2 = db.clone();
            tokio::spawn(async move { logger::run(db2).await });
            let addr: SocketAddr = format!("{bind}:{port}").parse()?;
            let backend =
                auth::resolve_password_backend(None).map_err(|e| anyhow::anyhow!("{e}"))?;
            server::serve(db, addr, backend).await?;
        }
        Cmd::Doctor => {
            let conn = db.lock();
            println!("db: {}", db_path.display());
            print!("{}", import::summarize(&conn)?);
        }
    }
    Ok(())
}
