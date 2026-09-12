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
            decode.push_str(&format!("let argument_{index} = ::loom::serde_json::from_value(values.remove(0)).map_err(|error| ::std::string::ToString::to_string(&error))?;\n"));
            arguments.push(format!("argument_{index}"));
        }
        generated.push_str(&format!(r#"
#[unsafe(export_name = "loom_call_{name}")]
extern "C" fn __loom_call_{name}(pointer: ::core::primitive::u32, length: ::core::primitive::u32) -> ::core::primitive::u64 {{
    let invoke = || -> ::std::result::Result<::loom::serde_json::Value, ::std::string::String> {{
        let bytes = unsafe {{ ::loom::core::input(pointer, length) }};
        let value: ::loom::serde_json::Value = ::loom::decode_host(bytes)?;
        let mut values = match value {{
            ::loom::serde_json::Value::Array(values) => values,
            ::loom::serde_json::Value::Null if {count} == 0 => ::std::vec::Vec::new(),
            value if {count} == 1 => ::std::vec::Vec::from([value]),
            _ => return ::std::result::Result::Err("entry {name}: arguments must be an array".into()),
        }};
        if values.len() != {count} {{ return ::std::result::Result::Err("entry {name}: incorrect argument count".into()); }}
        {decode}
        ::loom::serde_json::to_value(crate::{name}({arguments})).map_err(|error| ::std::string::ToString::to_string(&error))
    }};
    ::loom::core::response(invoke())
}}
"#, arguments = arguments.join(",")));
    }
    if let Some(schema) = &contract.schema {
        generated.push_str(&format!(
            r#"
#[unsafe(export_name = "loom_schema")]
extern "C" fn __loom_schema() -> ::core::primitive::u64 {{ ::loom::core::response(::std::result::Result::Ok({schema:?})) }}
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
    #[test]
    fn generated_wrapper_compiles_and_executes_entry_named_values() {
        let source = r#"
extern crate self as loom;
pub fn values() -> i32 { 42 }
struct Result;
struct String;
struct Vec;
struct u32;
struct u64;
fn Err() {}
fn Ok() {}
pub mod serde_json {
    pub enum Value { Array(std::vec::Vec<Value>), Null, Number(i32) }
    pub fn to_value(value: i32) -> std::result::Result<Value, std::string::String> {
        std::result::Result::Ok(Value::Number(value))
    }
}
pub fn decode_host(_: &[u8]) -> std::result::Result<serde_json::Value, std::string::String> {
    std::result::Result::Ok(serde_json::Value::Null)
}
pub mod core {
    pub unsafe fn input(_: u32, _: u32) -> &'static [u8] { &[] }
    pub fn response(result: std::result::Result<super::serde_json::Value, std::string::String>) -> u64 {
        match result.unwrap() {
            super::serde_json::Value::Number(value) => value as u64,
            _ => panic!("expected entry output"),
        }
    }
}
fn main() { assert_eq!(__loom_call_values(0, 0), 42); }
"#;
        let contract = Contract {
            entry: BTreeMap::from([("values".into(), "entry".into())]),
            schema: None,
        };
        let directory =
            std::env::temp_dir().join(format!("loom-entry-shadowing-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let input = directory.join("guest.rs");
        let executable = directory.join("guest");
        std::fs::write(&input, generate(source, &contract).unwrap()).unwrap();
        let output = std::process::Command::new("rustc")
            .args(["--edition=2024", "-A", "warnings"])
            .arg(&input)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::process::Command::new(&executable)
                .status()
                .unwrap()
                .success()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
