//! Host-owned ABI generation after the driver has resolved the guest contract.
use super::*;

#[derive(serde::Deserialize)]
struct Contract {
    entry: BTreeMap<String, String>,
    schema: Option<String>,
}

pub(super) struct Request<'a> {
    pub recipe: &'a Recipe,
    pub identity: &'a Path,
    pub root: &'a Path,
    pub cache: &'a Path,
    pub target: &'a Path,
    pub isolated: bool,
}

pub(super) async fn compile(request: Request<'_>) -> Result<(), BuildError> {
    let Request {
        recipe,
        identity,
        root,
        cache,
        target,
        isolated,
    } = request;
    let contract: Contract =
        serde_json::from_slice(&fs::read(identity.join("items.json")).await?).map_err(rejected)?;
    let source_path = recipe.source.join("src/lib.rs");
    let source = fs::read_to_string(&source_path).await?;
    let generated = generate(&source, &contract)?;
    fs::write(&source_path, generated).await?;
    let mut recipe = recipe.clone();
    recipe
        .arguments
        .retain(|argument| argument != "-Funsafe-code");
    // Identity is the original resolved guest HIR. The trusted adapters are
    // compiled by the same driver but are not guest items in its document.
    recipe.environment.remove("LOOM_ITEM_HASHES");
    recipe.environment.remove("LOOM_ITEM_PREIMAGES");
    let mut command = if isolated {
        fs::write(target.join("direct.sh"), recipe.shell()).await?;
        let mut command = Command::new(root.join("rustc/sandbox.sh"));
        command
            .arg("rustc")
            .arg(cache)
            .arg(recipe.working_directory())
            .arg(target)
            .arg(root);
        command
    } else {
        let mut command = Command::new(&recipe.compiler);
        command
            .args(&recipe.arguments)
            .current_dir(recipe.working_directory());
        command
    };
    compiler_environment(&mut command);
    command.env("RUSTC", &recipe.compiler);
    if !isolated {
        command.envs(&recipe.environment);
    }
    command
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES");
    let output = run(command).await;
    // Do not let generated wrappers enter a later build's source identity.
    fs::write(&source_path, source).await?;
    let output = output?;
    if !output.status.success() {
        return Err(rejected(format!(
            "entry ABI compiler {}: {}",
            recipe.compiler,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn generate(source: &str, contract: &Contract) -> Result<String, BuildError> {
    let file = syn::parse_file(source).map_err(rejected)?;
    let mut generated = source.to_owned();
    for name in contract.entry.keys() {
        let function = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function)
                    if function.sig.ident == name.as_str()
                        && matches!(function.vis, syn::Visibility::Public(_)) =>
                {
                    Some(function)
                }
                _ => None,
            })
            .ok_or_else(|| rejected(format!("driver entry {name} is not a root pub fn")))?;
        if function.sig.asyncness.is_some() || !function.sig.generics.params.is_empty() {
            return Err(rejected(format!(
                "entry {name} must be synchronous and non-generic"
            )));
        }
        let count = function.sig.inputs.len();
        let mut arguments = Vec::new();
        let mut decode = String::new();
        for index in 0..count {
            decode.push_str(&format!("let argument_{index} = ::loom::serde_json::from_value(values.remove(0)).map_err(|error| error.to_string())?;\n"));
            arguments.push(format!("argument_{index}"));
        }
        generated.push_str(&format!(r#"
#[unsafe(export_name = "loom_call_{name}")]
extern "C" fn __loom_call_{name}(pointer: u32, length: u32) -> u64 {{
    let invoke = || -> Result<::loom::serde_json::Value, String> {{
        let bytes = unsafe {{ ::loom::core::input(pointer, length) }};
        let value: ::loom::serde_json::Value = ::loom::decode_host(bytes)?;
        let mut values = match value {{
            ::loom::serde_json::Value::Array(values) => values,
            ::loom::serde_json::Value::Null if {count} == 0 => Vec::new(),
            value if {count} == 1 => Vec::from([value]),
            _ => return Err("entry {name}: arguments must be an array".into()),
        }};
        if values.len() != {count} {{ return Err("entry {name}: incorrect argument count".into()); }}
        {decode}
        ::loom::serde_json::to_value({name}({arguments})).map_err(|error| error.to_string())
    }};
    ::loom::core::response(invoke())
}}
"#, arguments = arguments.join(",")));
    }
    if let Some(schema) = &contract.schema {
        generated.push_str(&format!(
            r#"
#[unsafe(export_name = "loom_schema")]
extern "C" fn __loom_schema() -> u64 {{ ::loom::core::response(Ok({schema:?})) }}
"#
        ));
    }
    Ok(generated)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exports_only_driver_entries_and_evaluated_schema() {
        let source = "pub fn one(x: i32) -> i32 { x } pub fn two() -> i32 { 2 }";
        let contract = Contract {
            entry: BTreeMap::from([("one".into(), "a".into()), ("two".into(), "b".into())]),
            schema: Some("SELECT \\".into()),
        };
        let generated = generate(source, &contract).unwrap();
        syn::parse_file(&generated).unwrap();
        assert!(generated.contains("loom_call_one"));
        assert!(generated.contains("loom_call_two"));
        assert!(generated.contains("loom_schema"));
        assert!(!generated.contains("export_name = \"loom_call\""));
    }
}
