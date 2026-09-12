use crate::BuildError;
use std::{ffi::OsString, path::PathBuf, process::ExitCode};

struct Invocation {
    operation: &'static str,
    recipe: PathBuf,
    mirror: PathBuf,
}

fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Option<Invocation>, BuildError> {
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("__compiler-cache")) {
        return Ok(None);
    }
    let rejected = |message: &str| BuildError::Rejected(format!("__compiler-cache: {message}"));
    let operation = match arguments
        .next()
        .as_deref()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("lookup") => "lookup",
        Some("record") => "record",
        _ => return Err(rejected("operation must be lookup or record")),
    };
    let recipe = arguments
        .next()
        .ok_or_else(|| rejected("missing recipe path"))?;
    let mirror = arguments
        .next()
        .ok_or_else(|| rejected("missing mirror path"))?;
    if arguments.next().is_some() {
        return Err(rejected("unexpected argument after mirror path"));
    }
    if recipe.is_empty() || mirror.is_empty() {
        return Err(rejected("recipe and mirror paths must be nonempty"));
    }
    Ok(Some(Invocation {
        operation,
        recipe: recipe.into(),
        mirror: mirror.into(),
    }))
}

/// Dispatch the compiler subprocess mode before starting an application runtime.
/// Returns `None` for ordinary application arguments; exit code 3 is a cache miss.
pub fn compiler_cache_entry() -> Result<Option<ExitCode>, BuildError> {
    let Some(invocation) = parse(std::env::args_os().skip(1))? else {
        return Ok(None);
    };
    let hit = crate::direct::compiler_cache::main(
        invocation.operation,
        &invocation.recipe,
        &invocation.mirror,
    )?;
    Ok(Some(if hit {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_arguments_are_exact_and_ordinary_arguments_are_untouched() {
        assert!(
            parse(["--root", "/checkout"].into_iter().map(OsString::from))
                .unwrap()
                .is_none()
        );
        let invocation = parse(
            [
                "__compiler-cache",
                "lookup",
                "/recipe with spaces",
                "/mirror",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(invocation.operation, "lookup");
        assert_eq!(invocation.recipe, PathBuf::from("/recipe with spaces"));
        assert_eq!(invocation.mirror, PathBuf::from("/mirror"));
        for arguments in [
            vec!["__compiler-cache"],
            vec!["__compiler-cache", "unknown", "recipe", "mirror"],
            vec!["__compiler-cache", "record", "recipe"],
            vec!["__compiler-cache", "lookup", "recipe", "mirror", "extra"],
            vec!["__compiler-cache", "lookup", "", "mirror"],
        ] {
            assert!(parse(arguments.into_iter().map(OsString::from)).is_err());
        }
    }
}
