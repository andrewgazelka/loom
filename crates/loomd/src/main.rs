mod cluster_worker;
mod static_files;
use anyhow::{Context, ensure};
use axum::serve::ListenerExt;
use clap::Parser;
use std::{path::PathBuf, process::ExitCode, sync::Arc};

fn main() -> anyhow::Result<ExitCode> {
    if let Some(status) = loom_build::compiler_cache_entry()? {
        return Ok(status);
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
    #[arg(long)]
    stdio: bool,
    #[arg(long, env = "LOOM_BACKUP_DIR")]
    backup_dir: Option<PathBuf>,
    /// Shared actor object store: local:<dir> or s3://<bucket>?endpoint=<url>&region=<region>.
    #[arg(long, requires_all = ["advertise", "cluster_key_file"])]
    store: Option<String>,
    /// Stable identity; defaults to the ULID persisted in the actors directory.
    #[arg(long, requires = "store")]
    node_id: Option<String>,
    /// Listener address reachable by other cluster nodes (host:port).
    #[arg(long, requires = "store")]
    advertise: Option<String>,
    /// File containing exactly 32 raw bytes, provisioned with mode 0600.
    #[arg(long, requires = "store")]
    cluster_key_file: Option<PathBuf>,
}
async fn serve() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = args.actor_config().await?;
    let shutdown = Shutdown::new()?;
    let authorizer = if let Some(path) = args.tokens_file {
        loom_api::Authorizer::new(serde_json::from_slice(&tokio::fs::read(path).await?)?)?
    } else {
        loom_api::Authorizer::single(
            args.token
                .ok_or_else(|| anyhow::anyhow!("token required"))?,
        )?
    };
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
    let service = loom_api::Service::new(
        loom_store::Store::open(args.db)?,
        args.root.canonicalize()?,
        vec![loom_proto::Lang::Rust, loom_proto::Lang::JavaScript],
    )?
    .with_backup_directory(backup_directory);
    let node = loom_actor::Node::new(
        actors_dir,
        service.actor_registry(),
        Arc::new(loom_actor::DefaultEffects),
        config,
    )
    .await?;
    let authorizer = authorizer.with_ingress_bearer(node.ingress_bearer());
    let service = Arc::new(service.with_actors(node.clone()));
    if args.stdio {
        let worker = cluster_worker::ClusterWorker::start(&node);
        let result = tokio::select! {
            result = loom_mcp::stdio(service, node.clone()) => result,
            _ = shutdown.wait() => Ok(()),
        };
        if let Some(worker) = worker {
            worker.finish(&node).await?;
        }
        return result;
    }
    let mcp = loom_api::protect(
        loom_mcp::router(service.clone(), node.clone()),
        authorizer.clone(),
    );
    let app = loom_api::router(service, authorizer.clone())
        .merge(mcp)
        .fallback_service(loom_api::protect_public(
            static_files::router(ui),
            authorizer,
        ));
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("loomd listening on {}", listener.local_addr()?);
    // MCP streams headers before its result. Nagle plus delayed acknowledgments
    // otherwise adds a ~40 ms floor even to an empty loopback request on Linux.
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            eprintln!("failed to disable TCP buffering for an accepted connection: {error}");
        }
    });
    let worker = cluster_worker::ClusterWorker::start(&node);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.wait())
        .await;
    if let Some(worker) = worker {
        worker.finish(&node).await?;
    }
    result?;
    Ok(())
}

impl Args {
    async fn actor_config(&self) -> anyhow::Result<loom_actor::Config> {
        let Some(store) = &self.store else {
            return Ok(loom_actor::Config::default());
        };
        let addr = self
            .advertise
            .as_ref()
            .context("--advertise is required with --store")?;
        let authority: axum::http::uri::Authority =
            addr.parse().context("--advertise must be host:port")?;
        ensure!(
            authority.port_u16().is_some_and(|port| port != 0)
                && !authority.host().is_empty()
                && !addr.contains('@'),
            "--advertise must be host:port with a nonzero port"
        );
        let key_path = self
            .cluster_key_file
            .as_ref()
            .context("--cluster-key-file is required with --store")?;
        let file = tokio::fs::File::open(key_path)
            .await
            .with_context(|| format!("--cluster-key-file: open {}", key_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file
                .metadata()
                .await
                .context("--cluster-key-file: read permissions")?
                .permissions()
                .mode();
            ensure!(
                mode & 0o777 == 0o600,
                "--cluster-key-file must have mode 0600"
            );
        }
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        file.take(33)
            .read_to_end(&mut bytes)
            .await
            .context("--cluster-key-file: read key")?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("--cluster-key-file must contain exactly 32 raw bytes"))?;
        if let Some(id) = &self.node_id {
            ensure!(!id.is_empty(), "--node-id must not be empty");
        }
        Ok(loom_actor::Config {
            store: Some(parse_store(store)?),
            cluster: Some(loom_actor::ClusterConfig {
                // Node::new resolves the empty default from _node.db.meta.node_id.
                node_id: self.node_id.clone().unwrap_or_default(),
                addr: addr.clone(),
                key,
            }),
            ..Default::default()
        })
    }
}

fn parse_store(value: &str) -> anyhow::Result<loom_actor::StoreConfig> {
    if let Some(path) = value.strip_prefix("local:") {
        ensure!(!path.is_empty(), "--store local: requires a directory");
        return Ok(loom_actor::StoreConfig::Local { path: path.into() });
    }
    let url = url::Url::parse(value)
        .context("--store must be local:<dir> or s3://<bucket>?endpoint=&region=")?;
    ensure!(
        url.scheme() == "s3",
        "--store must be local:<dir> or s3://<bucket>?endpoint=&region="
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "--store s3 URL must contain only a bucket and endpoint/region query"
    );
    let bucket = url
        .host_str()
        .filter(|value| !value.is_empty())
        .context("--store s3 URL requires a bucket")?
        .to_owned();
    let mut endpoint = None;
    let mut region = None;
    for (name, value) in url.query_pairs() {
        let field = match name.as_ref() {
            "endpoint" => &mut endpoint,
            "region" => &mut region,
            _ => anyhow::bail!("--store: unknown query field {name}"),
        };
        ensure!(
            field.is_none() && !value.is_empty(),
            "--store: {name} must appear once with a nonempty value"
        );
        *field = Some(value.into_owned());
    }
    let endpoint = endpoint.context("--store s3 URL requires endpoint=")?;
    let endpoint_url =
        url::Url::parse(&endpoint).context("--store endpoint must be an HTTP URL")?;
    ensure!(
        matches!(endpoint_url.scheme(), "http" | "https") && endpoint_url.host_str().is_some(),
        "--store endpoint must be an HTTP URL"
    );
    Ok(loom_actor::StoreConfig::S3 {
        endpoint,
        bucket,
        region: region.context("--store s3 URL requires region=")?,
    })
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
