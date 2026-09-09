use clap::Parser;
use std::{path::PathBuf, sync::Arc};
#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "loom.sqlite")]
    db: PathBuf,
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
#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let ui = args.root.join("loom-ui/build");
    let backup_directory = args.backup_dir.unwrap_or_else(|| {
        args.db
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("backups")
    });
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
            result = loom_mcp::stdio(service) => result,
            _ = shutdown.wait() => Ok(()),
        };
    }
    let mcp = loom_api::protect(loom_mcp::router(service.clone()), authorizer.clone());
    let app = loom_api::router(service, authorizer)
        .merge(mcp)
        .fallback_service(tower_http::services::ServeDir::new(ui));
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("loomd listening on {}", listener.local_addr()?);
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
