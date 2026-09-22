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
    recipe.environment.remove("LOOM_DEP_ITEMS");
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
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env_remove("LOOM_DEP_ITEMS");
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
        let arguments = (0..count)
            .map(|index| format!("argument_{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        // The payload is one DAG-CBOR array of `count` typed arguments, decoded
        // straight into a tuple whose element types rustc infers from the
        // entry's own parameter types. Arity 0 is the empty array `[(); 0]`
        // (`()` would be CBOR null); arity 1 is a one-tuple.
        let (pattern, tuple) = match count {
            0 => ("[]".to_owned(), "[(); 0]".to_owned()),
            _ => (
                format!("({arguments},)"),
                format!("({},)", vec!["_"; count].join(", ")),
            ),
        };
        // Codec passes in this wrapper: one decode of the arguments (the
        // caller encoded them once), one encode of the result (the caller
        // decodes it once). The host copies both payloads without decoding.
        generated.push_str(&format!(r#"
#[unsafe(export_name = "loom_call_{name}")]
extern "C" fn __loom_call_{name}(pointer: ::core::primitive::u32, length: ::core::primitive::u32) -> ::core::primitive::u64 {{
    let invoke = || -> ::std::result::Result<::std::vec::Vec<::core::primitive::u8>, ::loom::CallError> {{
        let bytes = unsafe {{ ::loom::core::input(pointer, length) }};
        let {pattern}: {tuple} = ::loom::isolated::decode_payload(bytes)?;
        ::loom::isolated::encode_payload(&crate::{name}({arguments}))
    }};
    ::loom::core::isolated_response(invoke())
}}
"#));
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
    fn wrapper_decodes_a_tuple_of_the_entry_arity() {
        let source = "pub fn zero() -> i32 { 0 } pub fn one(x: i32) -> i32 { x } pub fn two(x: i32, y: String) -> i32 { x }";
        let contract = Contract {
            entry: BTreeMap::from([
                ("zero".into(), "a".into()),
                ("one".into(), "b".into()),
                ("two".into(), "c".into()),
            ]),
            schema: None,
        };
        let generated = generate(source, &contract).unwrap();
        syn::parse_file(&generated).unwrap();
        assert!(generated.contains("let []: [(); 0] = ::loom::isolated::decode_payload(bytes)?;"));
        assert!(
            generated
                .contains("let (argument_0,): (_,) = ::loom::isolated::decode_payload(bytes)?;")
        );
        assert!(generated.contains(
            "let (argument_0, argument_1,): (_, _,) = ::loom::isolated::decode_payload(bytes)?;"
        ));
        assert!(
            generated
                .contains("::loom::isolated::encode_payload(&crate::two(argument_0, argument_1))")
        );
        assert!(
            !generated.contains("serde_json"),
            "the wrapper must not build a Value tree"
        );
    }

    /// The wrapper is compiled against a stand-in `loom` crate that mimics the
    /// SDK surface it uses (`core::input`, `isolated::decode_payload`,
    /// `isolated::encode_payload`, `core::isolated_response`, `CallError`)
    /// while shadowing prelude names the generated code must not rely on.
    #[test]
    fn generated_wrapper_compiles_and_executes_entry_named_values() {
        let source = r#"
extern crate self as loom;
pub fn values(x: i32, y: i32) -> i32 { x + y }
struct Result;
struct String;
struct Vec;
struct u32;
struct u64;
struct u8;
fn Err() {}
fn Ok() {}
#[derive(Debug)]
pub struct CallError;
pub mod isolated {
    pub fn decode_payload<T: Decode>(bytes: &[::core::primitive::u8]) -> ::std::result::Result<T, super::CallError> {
        ::std::result::Result::Ok(T::decode(bytes))
    }
    pub fn encode_payload(value: &i32) -> ::std::result::Result<::std::vec::Vec<::core::primitive::u8>, super::CallError> {
        ::std::result::Result::Ok(::std::vec::Vec::from([*value as ::core::primitive::u8]))
    }
    pub trait Decode { fn decode(bytes: &[::core::primitive::u8]) -> Self; }
    impl Decode for (i32, i32) {
        fn decode(bytes: &[::core::primitive::u8]) -> Self { (bytes[0] as i32, bytes[1] as i32) }
    }
}
pub mod core {
    static INPUT: [::core::primitive::u8; 2] = [40, 2];
    pub unsafe fn input(_: ::core::primitive::u32, _: ::core::primitive::u32) -> &'static [::core::primitive::u8] { &INPUT }
    pub fn isolated_response(result: ::std::result::Result<::std::vec::Vec<::core::primitive::u8>, super::CallError>) -> ::core::primitive::u64 {
        result.unwrap()[0] as ::core::primitive::u64
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
