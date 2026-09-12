use super::*;

pub(super) fn read_graph(store: &Store, key: &str) -> Result<Option<Recipe>, BuildError> {
    let hash = store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_build_graphs (key TEXT PRIMARY KEY, recipe_hash TEXT NOT NULL)")?;
        let mut statement = connection.prepare("SELECT recipe_hash FROM rust_build_graphs WHERE key=?")?;
        let mut rows = statement.query_map([key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }).map_err(rejected)?;
    let Some(hash) = hash else {
        return Ok(None);
    };
    store.get_value(&hash).map_err(rejected)
}

pub(super) fn write_graph(store: &Store, key: &str, recipe: &Recipe) -> Result<(), BuildError> {
    let hash = store
        .put_value("rust-build-recipe", recipe)
        .map_err(rejected)?;
    store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_build_graphs (key TEXT PRIMARY KEY, recipe_hash TEXT NOT NULL)")?;
        connection.execute("INSERT INTO rust_build_graphs VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET recipe_hash=excluded.recipe_hash", [key, &hash])?;
        Ok(())
    }).map_err(rejected)
}

pub(super) struct RepairContext<'a> {
    pub(super) store: &'a Store,
    pub(super) root: &'a Path,
    pub(super) cache: &'a Path,
    pub(super) directory: &'a Path,
    pub(super) target: &'a Path,
    pub(super) isolated: bool,
}

pub(super) async fn repair_units(
    units: &[artifacts::Unit],
    context: RepairContext<'_>,
) -> Result<usize, BuildError> {
    let mut invocations = 0;
    for unit in units {
        let mut missing = false;
        for output in &unit.outputs {
            if context
                .store
                .codec(&output.hash)
                .map_err(rejected)?
                .is_none()
            {
                missing = true;
                continue;
            }
            if let Ok(bytes) = std::fs::read(&output.path)
                && blake3::hash(&bytes).to_hex().as_str() == output.hash
            {
                continue;
            }
            let bytes = context
                .store
                .get(&output.hash)
                .map_err(rejected)?
                .ok_or_else(|| rejected("Rust artifact disappeared during restoration"))?;
            artifacts::restore(output, &bytes)?;
        }
        if !missing {
            continue;
        }
        let command = if context.isolated {
            fs::write(context.target.join("direct.sh"), unit.recipe.shell()).await?;
            let mut command = Command::new(context.root.join("rustc/sandbox.sh"));
            command
                .arg("rustc")
                .arg(context.cache)
                .arg(context.directory)
                .arg(context.target)
                .arg(context.root);
            compiler_environment(&mut command);
            command.env("RUSTC", &unit.recipe.compiler);
            command
        } else {
            let mut command = Command::new(&unit.recipe.compiler);
            compiler_environment(&mut command);
            command
                .args(&unit.recipe.arguments)
                .envs(&unit.recipe.environment)
                .current_dir(unit.recipe.working_directory());
            command
        };
        let output = run(command).await?;
        invocations += 1;
        if !output.status.success() {
            return Err(rejected(format!(
                "rebuild {}: {}",
                unit.name,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        for artifact in &unit.outputs {
            let bytes = fs::read(&artifact.path).await?;
            let hash = context
                .store
                .put("rust-artifact", &bytes)
                .map_err(rejected)?;
            if hash != artifact.hash {
                return Err(rejected(format!(
                    "non-reproducible artifact for {}: expected {}, got {hash}",
                    unit.name, artifact.hash
                )));
            }
        }
    }
    Ok(invocations)
}
