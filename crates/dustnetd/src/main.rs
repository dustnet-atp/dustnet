// This binary is long-running and remote-facing; a panic here ends every
// connection it is serving, not
// just the request that caused it. The argv indexing that used to sit below
// this line is gone — clap deleted it rather than it being converted by hand.
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use dustnet_server::{StaticServer, StaticServerConfig};

/// Bounded static ATP server.
///
/// Parsing is delegated to clap rather than hand-rolled over `env::args()`.
/// The previous loop indexed `args[index]` after advancing past a flag, which
/// is a panicking operation on remote-shaped input; `--help` and `--version`
/// also did not exist. Both problems have the same fix.
#[derive(Parser)]
#[command(name = "dustnetd", version, about, long_about = None, arg_required_else_help = true)]
struct Cli {
    /// Print packaging artifacts instead of serving.
    ///
    /// Hidden because it is packaging machinery, not something an operator
    /// runs: `make install` calls it. Generating from the same `Command` the
    /// parser is built from is the point — artifacts maintained separately
    /// drift the moment a flag is renamed.
    #[command(subcommand)]
    generate: Option<Generate>,

    /// Directory served as the site root.
    ///
    /// Optional in the type only: clap cannot express "required unless a
    /// subcommand was given", because a subcommand is not an argument. The
    /// requirement is enforced below, with the same exit code clap uses.
    site_directory: Option<PathBuf>,

    /// Address to listen on. Defaults to `0.0.0.0` under TLS and `127.0.0.1`
    /// under `--plaintext-loopback`, which is the only address that mode
    /// accepts.
    #[arg(long, value_name = "ADDR")]
    bind: Option<IpAddr>,

    /// Port to listen on.
    #[arg(long, default_value_t = dustnet_core::protocol::DEFAULT_PORT)]
    port: u16,

    /// PEM certificate chain.
    #[arg(long, value_name = "FILE", conflicts_with = "plaintext_loopback")]
    cert: Option<String>,

    /// PEM private key.
    #[arg(long, value_name = "FILE", conflicts_with = "plaintext_loopback")]
    key: Option<String>,

    /// Serve plaintext for local development. Loopback only.
    #[arg(long)]
    plaintext_loopback: bool,

    /// Maximum concurrent connections, server-wide.
    ///
    /// The process also needs `RLIMIT_NOFILE` above this value; the OS will not
    /// raise it on our behalf. See `docs/guides/production-support.md`.
    #[arg(long, value_name = "N", default_value_t = dustnet_server::DEFAULT_MAX_CONNECTIONS)]
    max_connections: usize,

    /// Maximum concurrent connections from any one source IP.
    #[arg(long, value_name = "N", default_value_t = dustnet_server::DEFAULT_MAX_CONNECTIONS_PER_IP)]
    max_connections_per_ip: usize,

    /// Log encoding.
    #[arg(long, value_enum, default_value_t = LogFormat::Json)]
    log_format: LogFormat,
}

#[derive(Subcommand)]
enum Generate {
    /// Print a shell completion script.
    #[command(hide = true)]
    Completions { shell: clap_complete::Shell },

    /// Print the roff man page.
    #[command(hide = true)]
    Manpage,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogFormat {
    Json,
    Human,
}

/// Exit code for a usage error, matching clap's own.
const USAGE_EXIT: i32 = 2;

/// Blocking-pool threads available to `tokio::fs`.
///
/// Every `tokio::fs` call is dispatched to this pool, and serving a page is
/// several of them. Tokio's own default happens to be this number today, but a
/// default is not a decision: pinning it here means the server's filesystem
/// concurrency cannot shift underneath us when tokio changes its mind, and
/// gives the next person a named place to change it.
const BLOCKING_THREADS: usize = 512;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(BLOCKING_THREADS)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: failed to start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    runtime.block_on(run());
}

async fn run() {
    let cli = Cli::parse();

    match cli.generate {
        Some(Generate::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "dustnetd",
                &mut std::io::stdout(),
            );
            return;
        }
        Some(Generate::Manpage) => {
            if let Err(error) = clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())
            {
                eprintln!("Failed to render the man page: {error}");
                std::process::exit(1);
            }
            return;
        }
        None => {}
    }

    let Some(site_directory) = cli.site_directory else {
        eprintln!(
            "error: a site directory is required\n\nUsage: dustnetd [OPTIONS] <SITE_DIRECTORY>"
        );
        std::process::exit(USAGE_EXIT);
    };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if cli.log_format == LogFormat::Human {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    }

    if !site_directory.is_dir() {
        tracing::error!("site directory does not exist or is not a directory");
        std::process::exit(USAGE_EXIT);
    }

    let config = if cli.plaintext_loopback {
        // The listener refuses a non-loopback plaintext bind, but refusing
        // here as well makes the restriction a stated argument error rather
        // than a bind failure the operator has to interpret.
        let address = cli.bind.unwrap_or(IpAddr::from([127, 0, 0, 1]));
        if !address.is_loopback() {
            eprintln!(
                "error: --bind {address} is not a loopback address; \
                 --plaintext-loopback serves unencrypted traffic and is \
                 restricted to loopback"
            );
            std::process::exit(USAGE_EXIT);
        }
        StaticServerConfig::bind_plaintext_loopback(site_directory, &address.to_string(), cli.port)
            .await
    } else {
        let (Some(cert), Some(key)) = (cli.cert, cli.key) else {
            eprintln!("error: --cert and --key are required without --plaintext-loopback");
            std::process::exit(USAGE_EXIT);
        };
        let address = cli.bind.unwrap_or(IpAddr::from([0, 0, 0, 0]));
        StaticServerConfig::bind_tls(site_directory, &address.to_string(), cli.port, &cert, &key)
            .await
    }
    .unwrap_or_else(|error| {
        tracing::error!(%error, "failed to bind server");
        std::process::exit(1);
    })
    .with_connection_limits(cli.max_connections, cli.max_connections_per_ip);

    let mut server = StaticServer::new(config);
    let shutdown = server.shutdown_handle();
    tokio::spawn(async move {
        #[cfg(unix)]
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            // Losing SIGTERM degrades shutdown rather than ending it: the
            // server still drains on SIGINT. Aborting here would take down a
            // serving process over a handler it may never need.
            Err(error) => {
                tracing::error!(%error, "SIGTERM handler unavailable; SIGINT still stops the server");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown.send(true);
    });
    if let Err(error) = server.run().await {
        tracing::error!(%error, "server failed");
        std::process::exit(1);
    }
}
