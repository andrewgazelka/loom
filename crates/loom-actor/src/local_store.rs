//! LocalFileSystem with OS-locked compare-and-swap publication.
use async_trait::async_trait;
use futures_core::Stream;
use object_store::{local::LocalFileSystem, path::Path, *};
use std::{fmt, fs::OpenOptions, path::PathBuf, pin::Pin};
type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

#[derive(Debug)]
pub(crate) struct ConditionalLocalStore {
    inner: LocalFileSystem,
    lock_path: PathBuf,
}
impl ConditionalLocalStore {
    pub(crate) fn new(path: &std::path::Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(path)?;
        Ok(Self { inner: LocalFileSystem::new_with_prefix(path)?.with_fsync(true), lock_path: path.join(".cas") })
    }
}
fn failure(source: std::io::Error) -> Error {
    Error::Generic { store: "ConditionalLocalStore", source: Box::new(source) }
}
fn unsupported(operation: &str) -> Error {
    Error::NotImplemented { operation: operation.into(), implementer: "ConditionalLocalStore".into() }
}
impl fmt::Display for ConditionalLocalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConditionalLocalStore({})", self.inner)
    }
}
#[async_trait]
impl ObjectStore for ConditionalLocalStore {
    async fn put_opts(&self, location: &Path, payload: PutPayload, mut opts: PutOptions) -> Result<PutResult> {
        let inner = self.inner.clone();
        let lock_path = self.lock_path.join(format!("{location}.lock"));
        let location = location.clone();
        let runtime = tokio::runtime::Handle::current();
        // A canceled caller must not release the lock while LocalFileSystem's
        // blocking atomic rename is still in flight. This closure owns both.
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(lock_path.parent().expect("object lock has a parent")).map_err(failure)?;
            let guard = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(lock_path).map_err(failure)?;
            guard.lock().map_err(failure)?;
            runtime.block_on(async move {
                if let PutMode::Update(expected) = &opts.mode {
                    let actual = inner.head(&location).await.map_err(|error| match error {
                        Error::NotFound { .. } => {
                            Error::Precondition { path: location.to_string(), source: "conditional write target does not exist".into() }
                        }
                        error => error,
                    })?;
                    if actual.e_tag != expected.e_tag || actual.version != expected.version {
                        return Err(Error::Precondition { path: location.to_string(), source: "conditional write version changed".into() });
                    }
                    opts.mode = PutMode::Overwrite;
                }
                inner.put_opts(&location, payload, opts).await
            })
        })
        .await
        .map_err(|error| failure(std::io::Error::other(error)))?
    }

    async fn put_multipart_opts(&self, _: &Path, _: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
        Err(unsupported("multipart"))
    }
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.inner.get_opts(location, options).await
    }
    fn delete_stream(&self, _: BoxStream<Result<Path>>) -> BoxStream<Result<Path>> {
        Box::pin(RefuseDelete { yielded: false })
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, _: &Path, _: &Path, _: CopyOptions) -> Result<()> {
        Err(unsupported("copy"))
    }
}
struct RefuseDelete {
    yielded: bool,
}
impl Stream for RefuseDelete {
    type Item = Result<Path>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        if self.yielded {
            return std::task::Poll::Ready(None);
        }
        self.yielded = true;
        std::task::Poll::Ready(Some(Err(unsupported("delete"))))
    }
}
