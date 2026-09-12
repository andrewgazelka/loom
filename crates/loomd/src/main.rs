mod static_files;
use axum::serve::ListenerExt;
use clap::Parser;
use std::{path::PathBuf, process::ExitCode, sync::Arc};

#[derive(Parser)]
struct CompilerCacheArgs {
    #[arg(value_enum)]
    operation: CompilerCacheOperation,
    recipe: PathBuf,
    mirror: PathBuf,
}

#[derive(Clone, clap::ValueEnum)]
enum CompilerCacheOperation {
    Lookup,
    Record,
}

fn main() -> anyhow::Result<ExitCode> {
    // Cargo invokes this internal mode once per compilation unit. Dispatch
    // before starting the daemon's executor or opening its application store.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__compiler-cache")) {
        let args = CompilerCacheArgs::parse_from(std::env::args_os().skip(1));
        let operation = match args.operation {
            CompilerCacheOperation::Lookup => "lookup",
            CompilerCacheOperation::Record => "record",
        };
        let hit = loom_build::compiler_cache_main(operation, &args.recipe, &args.mirror)?;
        return Ok(if hit {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(3)
        });
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())?;
    Ok(ExitCode::SUCCESS)
}
#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "loom.sqlite")]
    db: PathBuf,
    #[arg(long)]
    actors_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: std::net::SocketAddr,
    #[arg(
        long,
        env = "LOOM_TOKEN",
        required_unless_present = "tokens_file",
        conflicts_with = "tokens_file"
    )]
    token: Option<String>,
    #[arg(long)]
    tokens_file: Option<PathBuf>,
    #[arg(long, env = "LOOM_ROOT", default_value = ".")]
    root: PathBuf,
    #[arg(long, default_value = "ts,rust")]
    lang: String,
    #[arg(long)]
    stdio: bool,
    #[arg(long, env = "LOOM_BACKUP_DIR")]
    backup_dir: Option<PathBuf>,
}
async fn serve() -> anyhow::Result<()> {
    let args = Args::parse();
    let shutdown = Shutdown::new()?;
    let authorizer = if let Some(path) = args.tokens_file {
        loom_api::Authorizer::new(serde_json::from_slice(&tokio::fs::read(path).await?)?)?
    } else {
        loom_api::Authorizer::single(
            args.token
                .ok_or_else(|| anyhow::anyhow!("token required"))?,
        )?
    };
    let languages = args
        .lang
        .split(',')
        .map(|lang| match lang {
            "ts" => Ok(loom_proto::Lang::Ts),
            "rust" => Ok(loom_proto::Lang::Rust),
            _ => Err(anyhow::anyhow!("unknown language {lang}")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let ui = args.root.join("ui/build");
    let backup_directory = args.backup_dir.unwrap_or_else(|| {
        args.db
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("backups")
    });
    let actors_dir = args.actors_dir.unwrap_or_else(|| {
        args.db
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("actors")
    });
    let node = loom_actor::Node::new(
        actors_dir,
        loom_actor::Registry::new(),
        Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    let service = Arc::new(
        loom_api::Service::new(
            loom_store::Store::open(args.db)?,
            args.root.canonicalize()?,
            languages,
        )?
        .with_backup_directory(backup_directory),
    );
    if args.stdio {
        return tokio::select! {
            result = loom_mcp::stdio(service, node) => result,
            _ = shutdown.wait() => Ok(()),
        };
    }
    let mcp = loom_api::protect(loom_mcp::router(service.clone(), node), authorizer.clone());
    let app = loom_api::router(service, authorizer)
        .merge(mcp)
        .fallback_service(static_files::router(ui));
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("loomd listening on {}", listener.local_addr()?);
    // MCP streams headers before its result. Nagle plus delayed acknowledgments
    // otherwise adds a ~40 ms floor even to an empty loopback request on Linux.
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            eprintln!("failed to disable TCP buffering for an accepted connection: {error}");
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.wait())
        .await?;
    Ok(())
}

#[cfg(unix)]
struct Shutdown {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}
#[cfg(unix)]
impl Shutdown {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }
    async fn wait(mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {},
            _ = self.interrupt.recv() => {},
        }
    }
}
#[cfg(not(unix))]
struct Shutdown;
#[cfg(not(unix))]
impl Shutdown {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }
    async fn wait(self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}
