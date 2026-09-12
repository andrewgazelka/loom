use super::*;

impl Runtime {
    pub fn model(&self) -> &loom_model::Model {
        &self.inner.model
    }
    pub fn processes(&self) -> &loom_process::Supervisor {
        &self.inner.processes
    }
    pub(super) async fn execute_command(
        &self,
        command: tokio::process::Command,
        capture_paths: Vec<std::path::PathBuf>,
    ) -> Result<Value> {
        let native = command.as_std();
        let cwd = native
            .get_current_dir()
            .map(std::path::Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?);
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        );
        let spec = loom_process::ProcessSpec {
            machine: "local".into(),
            capture_paths,
            program: native
                .get_program()
                .to_str()
                .context("program must be UTF8")?
                .into(),
            args: native
                .get_args()
                .map(|arg| {
                    arg.to_str()
                        .map(str::to_owned)
                        .context("argument must be UTF8")
                })
                .collect::<Result<Vec<_>>>()?,
            root: cwd.clone(),
            cwd,
            env,
        };
        let process = self.inner.processes.start(spec).await?;
        let _cancel = self.inner.processes.cancel_on_drop(&process.id);
        let completed = tokio::time::timeout(
            Duration::from_secs(300),
            self.inner.processes.wait(&process.id),
        )
        .await??;
        if let Some(error) = completed.error {
            bail!("process failed: {error}");
        }
        if completed.phase != loom_process::Phase::Completed {
            bail!("process did not complete: {:?}", completed.phase);
        }
        Ok(
            json!({"code":completed.code,"stdout":completed.stdout,"stderr":completed.stderr,"filesystem_changes":completed.filesystem_changes,"filesystem_capture":completed.filesystem_capture}),
        )
    }
}
