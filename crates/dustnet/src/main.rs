// `connect` renders bytes a remote server chose, and the file subcommands
// render whatever AML they are
// handed; a panic in either leaves the terminal in raw mode. The argv indexing
// and the `unreachable!()` that used to sit below this line are gone — clap
// deleted them rather than them being converted by hand.
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

use std::env;
use std::fs;
use std::process;

use clap::{Args, CommandFactory, Parser, Subcommand};
use dustnet_client::compositor::layout::text::WidthConfig;

/// Detect the correct ambiguous width for this terminal.
///
/// Priority: DUSTNET_AMBIGUOUS_WIDTH env var > terminal probe > locale heuristic.
fn detect_width_config() -> WidthConfig {
    // 1. Explicit env var override
    if let Ok(val) = env::var("DUSTNET_AMBIGUOUS_WIDTH")
        && let Ok(w) = val.parse::<u8>()
        && (w == 1 || w == 2)
    {
        return WidthConfig { ambiguous_width: w };
    }

    // 2. Terminal probe (may not work in all environments)
    if let Some(w) = dustnet_client::compositor::present::probe::probe_ambiguous_width() {
        return WidthConfig { ambiguous_width: w };
    }

    // 3. Locale heuristic
    let w = dustnet_client::compositor::present::probe::locale_ambiguous_width();
    WidthConfig { ambiguous_width: w }
}

// ─── CLI Parsing ─────────────────────────────────────────────
//
// Parsing is delegated to clap rather than hand-rolled over `env::args()`.
// The previous `parse_args` indexed `args[1]`, `args[2]` and `args[3..]`
// after length checks — correct, but correct by an argument no gate reads —
// and ended in an `unreachable!()` that a second match arm could reach.
// `--help` and `--version` also did not exist. All three have the same fix.

#[derive(Parser)]
#[command(name = "dustnet", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render a page to the terminal.
    Render { file: String },
    /// Parse and validate, reporting errors.
    Check { file: String },
    /// Scan and print tokens.
    DumpTokens { file: String },
    /// Parse and print the AST.
    DumpAst { file: String },
    /// Lay out and print plain text.
    DumpCells { file: String },
    /// Build the scene graph and print its tree.
    DumpScene { file: String },
    /// Connect to an ATP server.
    Connect(ConnectOpts),

    /// Inspect and edit pinned site certificates.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },

    /// Print a shell completion script.
    ///
    /// Hidden because it is packaging machinery, not something a user runs:
    /// `make install` calls it. Generating from the same `Command` the parser
    /// is built from is the point — a completion script maintained separately
    /// drifts the moment a subcommand is renamed.
    #[command(hide = true)]
    Completions { shell: clap_complete::Shell },

    /// Print the roff man page.
    #[command(hide = true)]
    Manpage,
}

impl Command {
    /// The file a command reads, or `None` for `connect`.
    ///
    /// Returned rather than matched at each use site so a new file-taking
    /// subcommand cannot be silently omitted from the dispatch in `main`.
    fn file(&self) -> Option<&str> {
        match self {
            Self::Render { file }
            | Self::Check { file }
            | Self::DumpTokens { file }
            | Self::DumpAst { file }
            | Self::DumpCells { file }
            | Self::DumpScene { file } => Some(file),
            Self::Connect(_) | Self::Trust { .. } | Self::Completions { .. } | Self::Manpage => {
                None
            }
        }
    }
}

#[derive(Args)]
struct ConnectOpts {
    /// `atp://host[:port]/path`
    ///
    /// Held as a string and parsed in `run_connect`: a clap `value_parser`
    /// requires its output to be `Clone`, and `AtpUri` owns heap fields that
    /// the allocation registry governs. Adding `Clone` to it to satisfy an
    /// argument parser would be the tail wagging the dog.
    uri: String,

    /// Use plaintext instead of TLS. Loopback only.
    #[arg(long, conflicts_with_all = ["insecure", "tofu", "ca_file"])]
    no_tls: bool,

    /// Accept any server certificate, without pinning it.
    ///
    /// Encrypted and unauthenticated: no protection against an active man in
    /// the middle, on this connection or any later one. Prefer `--tofu`, which
    /// is equally convenient on the first connection and actually
    /// authenticates every one after it.
    #[arg(long, conflicts_with_all = ["tofu", "ca_file"])]
    insecure: bool,

    /// Pin this site's certificate on first use, and require it thereafter.
    ///
    /// Only needed once per site: a pin is honoured on later connections
    /// without the flag. Manage pins with `dustnet trust`.
    #[arg(long)]
    tofu: bool,

    /// Trust an additional certificate authority, as a PEM file.
    ///
    /// Added to the built-in bundle rather than replacing it, and host names
    /// are verified as usual.
    #[arg(long, value_name = "PEM")]
    ca_file: Option<String>,
}

#[derive(Subcommand)]
enum TrustCommand {
    /// List pinned sites.
    List,
    /// Forget a pinned site, so the next `--tofu` connection pins it again.
    Forget {
        /// `host[:port]`, defaulting to the ATP port.
        site: String,
    },
}

/// The async runtime, or a stated failure.
///
/// Runtime construction fails on thread or file-descriptor exhaustion, which
/// is a real condition on a loaded machine rather than a programming error, so
/// it gets a message a user can act on instead of a panic backtrace.
fn tokio_runtime() -> tokio::runtime::Runtime {
    match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Failed to start the async runtime: {error}");
            process::exit(1);
        }
    }
}

// ─── Command execution ──────────────────────────────────────

fn run_file_command(filename: &str, command: &Command) {
    let bytes = match fs::read(filename) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error reading {filename}: {e}");
            process::exit(1);
        }
    };
    let mut scanner = match dustnet_core::scanner::Scanner::new(&bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Scanner error: {e}");
            process::exit(1);
        }
    };
    let tokens = match scanner.scan_all() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Scanner error: {e}");
            process::exit(1);
        }
    };

    if matches!(command, Command::DumpTokens { .. }) {
        for (i, token) in tokens.iter().enumerate() {
            println!("{i:4}: {token:?}");
        }
        println!("\n{} tokens total", tokens.len());
        return;
    }

    let result = dustnet_core::parser::parse(tokens);
    for diag in &result.diagnostics {
        eprintln!("{diag}");
    }

    if matches!(command, Command::Check { .. }) {
        if result.has_errors() {
            let error_count = result
                .diagnostics
                .iter()
                .filter(|d| d.level == dustnet_core::parser::DiagnosticLevel::Error)
                .count();
            let warning_count = result.diagnostics.len() - error_count;
            eprintln!("\n{filename}: {error_count} error(s), {warning_count} warning(s)");
            process::exit(1);
        } else {
            let warning_count = result.diagnostics.len();
            if warning_count > 0 {
                println!("{filename}: OK ({warning_count} warning(s))");
            } else {
                println!("{filename}: OK");
            }
        }
        return;
    }

    if matches!(command, Command::DumpAst { .. }) {
        if let Some(doc) = &result.document {
            println!("{doc:#?}");
        } else {
            eprintln!("No document produced.");
            process::exit(1);
        }
        return;
    }

    let doc = match result.document {
        Some(d) => d,
        None => {
            eprintln!("Failed to parse document.");
            process::exit(1);
        }
    };

    if matches!(command, Command::DumpScene { .. }) {
        let scene = dustnet_client::compositor::scene::build::from_document(&doc);
        print!("{}", scene.debug_dump());
        return;
    }

    let color_support = dustnet_client::compositor::terminal::detect_color_support();
    let wcfg = detect_width_config();

    if matches!(command, Command::DumpCells { .. }) {
        let (term_w, term_h) =
            dustnet_client::compositor::terminal::Terminal::size().unwrap_or((80, 24));
        let mut scene = dustnet_client::compositor::scene::build::from_document(&doc);
        let dimensions = dustnet_client::compositor::layout::engine::layout_scene(
            &mut scene,
            term_w,
            term_h,
            color_support,
            wcfg,
        )
        .buffer;
        let animations = dustnet_client::compositor::animate::runtime::AnimationRuntime::empty();
        let buf = dustnet_client::compositor::composite::walk(
            &scene,
            &animations,
            dimensions.width,
            dimensions.height,
        );
        print!(
            "{}",
            dustnet_client::compositor::present::render_to_string(&buf)
        );
    } else {
        let config = dustnet_client::config::load_config();
        let wasm_dir = std::path::Path::new(filename).parent();
        let rt = tokio_runtime();
        if let Err(e) = rt.block_on(dustnet_client::compositor::terminal::run_viewer(
            &doc,
            color_support,
            wcfg,
            &config,
            wasm_dir,
        )) {
            eprintln!("Render error: {e}");
            process::exit(1);
        }
    }
}

fn run_connect(opts: ConnectOpts) {
    let uri = match dustnet_core::protocol::uri::AtpUri::parse(&opts.uri) {
        Ok(uri) => uri,
        Err(error) => {
            eprintln!("Invalid URI: {error}");
            process::exit(1);
        }
    };
    let color_support = dustnet_client::compositor::terminal::detect_color_support();
    let wcfg = detect_width_config();
    let config = dustnet_client::config::load_config();
    let policy = build_tls_policy(&opts);

    let rt = tokio_runtime();
    if let Err(e) = rt.block_on(dustnet_client::compositor::terminal::run_connected_viewer(
        &uri,
        policy,
        color_support,
        wcfg,
        &config,
    )) {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

/// Turn the mutually exclusive trust flags into one policy.
///
/// The store is loaded even when no trust flag was passed, because a pin is a
/// decision the user already made and it governs from then on. Loading it is
/// fallible and the failure is fatal: a store that cannot be read is not a
/// store with nothing in it, and continuing would silently fall back to CA
/// verification for a site the user had deliberately pinned.
fn build_tls_policy(opts: &ConnectOpts) -> dustnet_client::TlsPolicy {
    let mode = if opts.no_tls {
        dustnet_client::TlsMode::PlaintextLoopback
    } else if opts.insecure {
        dustnet_client::TlsMode::Insecure
    } else if opts.tofu {
        dustnet_client::TlsMode::TrustOnFirstUse
    } else {
        dustnet_client::TlsMode::Verified
    };

    let store = match dustnet_client::trust::TrustStore::load() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Error: {error}");
            process::exit(1);
        }
    };

    let mut policy = dustnet_client::TlsPolicy::new(mode, store);
    if let Some(path) = &opts.ca_file {
        policy = match policy.with_ca_file(std::path::Path::new(path)) {
            Ok(policy) => policy,
            Err(error) => {
                eprintln!("Error: {error}");
                process::exit(1);
            }
        };
    }
    policy
}

fn run_trust(command: &TrustCommand) {
    let mut store = match dustnet_client::trust::TrustStore::load() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Error: {error}");
            process::exit(1);
        }
    };
    match command {
        TrustCommand::List => {
            if store.is_empty() {
                println!("No pinned sites. Pin one with: dustnet connect <uri> --tofu");
                return;
            }
            for (host, port, pin) in store.iter() {
                println!(
                    "{host}:{port} {} first-seen={}",
                    pin.fingerprint, pin.first_seen
                );
            }
        }
        TrustCommand::Forget { site } => {
            let (host, port) = split_site(site);
            match store.forget(host, port) {
                Ok(true) => println!("Forgot {host}:{port}"),
                Ok(false) => {
                    eprintln!("{host}:{port} was not pinned");
                    process::exit(1);
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    process::exit(1);
                }
            }
        }
    }
}

/// Split `host[:port]`, defaulting to the ATP port.
///
/// An IPv6 literal is bracketed, so the last colon only separates a port when
/// it follows the closing bracket.
fn split_site(site: &str) -> (&str, u16) {
    let default = dustnet_core::protocol::DEFAULT_PORT;
    if let Some(rest) = site.strip_prefix('[') {
        return match rest.split_once("]:") {
            Some((host, port)) => (host, port.parse().unwrap_or(default)),
            None => (rest.trim_end_matches(']'), default),
        };
    }
    match site.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host, port),
            Err(_) => (site, default),
        },
        _ => (site, default),
    }
}

// ─── Main ────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "dustnet",
                &mut std::io::stdout(),
            );
        }
        Command::Manpage => {
            if let Err(error) = clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())
            {
                eprintln!("Failed to render the man page: {error}");
                process::exit(1);
            }
        }
        Command::Connect(opts) => run_connect(opts),
        Command::Trust { command } => run_trust(&command),
        command => {
            let file = command.file().unwrap_or_default().to_string();
            run_file_command(&file, &command);
        }
    }
}
