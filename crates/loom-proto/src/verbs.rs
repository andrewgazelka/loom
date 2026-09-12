//! The public command vocabulary and transport-independent argument schemas.
use serde_json::{Value, json};

pub type ValidationCount = u32;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Definition,
    Actor,
    Internal,
}
#[derive(Clone, Copy)]
pub enum Permission {
    Read,
    Execute,
    Define,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    String,
    Source,
    Json,
    Integer,
    Count,
}
pub struct Argument {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
    pub flag: bool,
    pub default: Option<&'static str>,
}
pub struct Verb {
    pub name: &'static str,
    pub family: Family,
    pub permission: Permission,
    pub arguments: &'static [Argument],
}
macro_rules! arg {
    ($name:ident, $kind:ident) => {
        Argument {
            name: stringify!($name),
            kind: Kind::$kind,
            required: true,
            flag: false,
            default: None,
        }
    };
    ($name:ident, $kind:ident, optional) => {
        Argument {
            name: stringify!($name),
            kind: Kind::$kind,
            required: false,
            flag: true,
            default: None,
        }
    };
    ($name:ident, $kind:ident, flag) => {
        Argument {
            name: stringify!($name),
            kind: Kind::$kind,
            required: true,
            flag: true,
            default: None,
        }
    };
    ($name:ident, $kind:ident, $default:literal) => {
        Argument {
            name: stringify!($name),
            kind: Kind::$kind,
            required: false,
            flag: false,
            default: Some($default),
        }
    };
}
macro_rules! verb {
    ($name:ident, $family:ident, $permission:ident, [$($argument:expr),*]) => {
        Verb {name:stringify!($name),family:Family::$family,permission:Permission::$permission,arguments:&[$($argument),*]}
    };
}
pub static VERBS: &[Verb] = &[
    verb!(
        command,
        Internal,
        Read,
        [arg!(command, String), arg!(args, Json, "{}")]
    ),
    verb!(
        add,
        Definition,
        Define,
        [
            arg!(source, Source),
            arg!(name, String, optional),
            arg!(deps, Json, optional),
            arg!(allowed_effects, Json, optional)
        ]
    ),
    verb!(view, Definition, Read, [arg!(target, String)]),
    verb!(
        update,
        Definition,
        Define,
        [
            arg!(name, String),
            arg!(source, Source),
            arg!(deps, Json, optional),
            arg!(allowed_effects, Json, optional)
        ]
    ),
    verb!(history, Definition, Read, [arg!(name, String)]),
    verb!(
        diff,
        Definition,
        Read,
        [arg!(old, String), arg!(new, String)]
    ),
    verb!(
        run,
        Definition,
        Execute,
        [arg!(target, String), arg!(args, Json, "[]")]
    ),
    verb!(find, Definition, Read, [arg!(text, String)]),
    verb!(dependents, Definition, Read, [arg!(hash, String)]),
    verb!(
        spawn,
        Actor,
        Execute,
        [
            arg!(def, String),
            arg!(init, Json, "null"),
            arg!(parent, String, optional),
            arg!(spec, Json, optional)
        ]
    ),
    verb!(
        send,
        Actor,
        Execute,
        [
            arg!(id, String),
            arg!(msg, Json),
            arg!(key, String, optional)
        ]
    ),
    verb!(tree, Actor, Read, [arg!(root, String, optional)]),
    verb!(info, Actor, Read, [arg!(id, String)]),
    verb!(lineage, Actor, Read, [arg!(id, String)]),
    verb!(
        validate,
        Actor,
        Execute,
        [
            arg!(id, String),
            arg!(candidate, String),
            arg!(k, Count),
            arg!(assertions, Json, optional)
        ]
    ),
    verb!(
        promote,
        Actor,
        Define,
        [
            arg!(id, String),
            arg!(hash, String),
            arg!(rationale, String, flag),
            arg!(author, String, flag)
        ]
    ),
    verb!(fork, Actor, Execute, [arg!(id, String), arg!(seq, Integer)]),
    verb!(actors, Actor, Read, []),
    verb!(
        stop,
        Actor,
        Execute,
        [arg!(id, String), arg!(reason, String)]
    ),
    verb!(
        restart,
        Actor,
        Execute,
        [arg!(id, String), arg!(verb, String)]
    ),
    verb!(dead_letters, Actor, Read, [arg!(id, String)]),
    verb!(
        sql,
        Actor,
        Read,
        [
            arg!(id, String),
            arg!(query, String),
            arg!(params, Json, optional)
        ]
    ),
    verb!(whereis, Actor, Read, [arg!(name, String)]),
    verb!(
        register,
        Actor,
        Execute,
        [arg!(name, String), arg!(id, String)]
    ),
    verb!(members, Actor, Read, [arg!(group, String)]),
    verb!(behaviors, Actor, Read, []),
    verb!(
        promote_where,
        Actor,
        Define,
        [
            arg!(old, String),
            arg!(new, String),
            arg!(rationale, String, flag),
            arg!(author, String, flag)
        ]
    ),
    verb!(drain, Actor, Execute, []),
];
pub fn lookup(name: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|verb| verb.name == name)
}
impl Verb {
    pub fn schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for argument in self.arguments {
            let mut schema = match argument.kind {
                Kind::String | Kind::Source if !argument.required => {
                    json!({"type":["string","null"]})
                }
                Kind::String | Kind::Source => json!({"type":"string"}),
                Kind::Json => json!({}),
                Kind::Integer => json!({"type":"integer"}),
                Kind::Count => json!({"type":"integer","minimum":0,"maximum":ValidationCount::MAX}),
            };
            if let Some(default) = argument.default {
                schema["default"] = serde_json::from_str(default).expect("verb default is JSON");
            }
            properties.insert(argument.name.into(), schema);
            if argument.required {
                required.push(argument.name);
            }
        }
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
    pub fn normalize(&self, args: &mut Value) -> Result<(), String> {
        let object = args
            .as_object_mut()
            .ok_or_else(|| format!("{} arguments must be an object", self.name))?;
        for name in object.keys() {
            if !self.arguments.iter().any(|arg| arg.name == name) {
                return Err(format!("{} unknown argument {name}", self.name));
            }
        }
        for argument in self.arguments {
            if !object.contains_key(argument.name) {
                if let Some(default) = argument.default {
                    object.insert(
                        argument.name.into(),
                        serde_json::from_str(default).expect("verb default is JSON"),
                    );
                } else if argument.required {
                    return Err(format!("{} requires {}", self.name, argument.name));
                } else {
                    continue;
                }
            }
            let value = &object[argument.name];
            let valid = match argument.kind {
                Kind::String | Kind::Source => {
                    value.is_string() || (!argument.required && value.is_null())
                }
                Kind::Json => true,
                Kind::Integer => value.as_i64().is_some(),
                Kind::Count => value
                    .as_u64()
                    .is_some_and(|value| ValidationCount::try_from(value).is_ok()),
            };
            if !valid {
                return Err(format!("{} invalid argument {}", self.name, argument.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_defaults_and_validation_bounds_are_shared() {
        let mut spawn = json!({"def":"counter"});
        lookup("spawn").unwrap().normalize(&mut spawn).unwrap();
        assert_eq!(spawn["init"], Value::Null);
        let mut valid = json!({"id":"a","candidate":"h","k":ValidationCount::MAX});
        lookup("validate").unwrap().normalize(&mut valid).unwrap();
        for invalid in [
            json!(-1),
            json!(1.5),
            json!(u64::from(ValidationCount::MAX) + 1),
        ] {
            valid["k"] = invalid;
            assert!(lookup("validate").unwrap().normalize(&mut valid).is_err());
        }
    }

    #[test]
    fn optional_strings_accept_null_in_schema_and_normalization() {
        for name in ["tree", "spawn", "send", "add"] {
            let verb = lookup(name).unwrap();
            let schema = verb.schema();
            let mut args = match name {
                "spawn" => json!({"def":"counter"}),
                "send" => json!({"id":"a", "msg":{}}),
                "add" => json!({"source":"pub fn main() {}"}),
                _ => json!({}),
            };
            for argument in verb
                .arguments
                .iter()
                .filter(|argument| !argument.required && argument.kind == Kind::String)
            {
                assert_eq!(
                    schema["properties"][argument.name]["type"],
                    json!(["string", "null"])
                );
                args[argument.name] = Value::Null;
            }
            verb.normalize(&mut args).unwrap();
        }
        let mut args = json!({"id":null});
        let verb = lookup("info").unwrap();
        assert_eq!(verb.schema()["properties"]["id"]["type"], "string");
        assert!(verb.normalize(&mut args).is_err());
    }

    #[test]
    fn promotion_requires_author_and_rejects_retired_arguments() {
        let mut promotion = json!({"id":"a","hash":"h","rationale":"test"});
        assert_eq!(
            lookup("promote")
                .unwrap()
                .normalize(&mut promotion)
                .unwrap_err(),
            "promote requires author"
        );
        promotion["author"] = json!("owner");
        lookup("promote")
            .unwrap()
            .normalize(&mut promotion)
            .unwrap();
        promotion["behavior_hash"] = json!("h");
        assert!(
            lookup("promote")
                .unwrap()
                .normalize(&mut promotion)
                .is_err()
        );
    }
}
