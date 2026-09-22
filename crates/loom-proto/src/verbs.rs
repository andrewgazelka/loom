//! The public command vocabulary and transport-independent argument schemas.
use serde_json::{Value, json};
#[cfg(test)]
#[path = "verbs_view_tests.rs"]
mod view_tests;

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
    /// UTF-8 text; the CLI reads it from a file path.
    Source,
    Json,
    Integer,
    Count,
    Boolean,
    /// Object of string values; the CLI collects repeated `--flag key=value`
    /// pairs and also accepts the singular flag spelling (`--dep` for `deps`).
    Map,
    /// Array of strings; the CLI collects repeated positional words.
    List,
    /// Raw CAS reference (CID) of an uploaded object; the CLI uploads the file
    /// at the given path through `POST /v1/cas` and sends the returned CID.
    Upload,
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
            Argument {
                name: "lang",
                kind: Kind::String,
                required: false,
                flag: true,
                default: Some("\"typescript\"")
            },
            arg!(deps, Map, optional),
            arg!(allowed_effects, Json, optional)
        ]
    ),
    verb!(
        view,
        Definition,
        Read,
        [
            arg!(target, String, optional),
            arg!(actor, String, optional),
            arg!(table, String, optional),
            arg!(template, String, optional),
            arg!(order_by, Json, optional)
        ]
    ),
    verb!(
        update,
        Definition,
        Define,
        [
            arg!(name, String),
            arg!(source, Source),
            arg!(deps, Map, optional),
            arg!(allowed_effects, Json, optional),
            arg!(expected_hash, String, optional),
            arg!(request_id, String, optional)
        ]
    ),
    verb!(update_view, Definition, Read, [arg!(id, String)]),
    verb!(
        update_repair,
        Definition,
        Define,
        [arg!(id, String), arg!(revision, Count), arg!(changes, Json)]
    ),
    verb!(
        update_abort,
        Definition,
        Define,
        [arg!(id, String), arg!(revision, Count)]
    ),
    verb!(
        update_rebase,
        Definition,
        Define,
        [arg!(id, String), arg!(revision, Count)]
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
    verb!(export, Definition, Read, [arg!(targets, List)]),
    verb!(
        import,
        Definition,
        Define,
        [arg!(bundle, Upload), arg!(into, String, optional)]
    ),
    verb!(
        spawn,
        Actor,
        Execute,
        [
            arg!(def, String),
            arg!(init, Json, "null"),
            arg!(parent, String, optional),
            arg!(spec, Json, optional),
            arg!(durability, String, optional)
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
    verb!(subscriptions, Actor, Read, [arg!(id, String)]),
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
    verb!(actors, Actor, Read, [arg!(cluster, Boolean, optional)]),
    verb!(nodes, Actor, Read, []),
    verb!(
        move,
        Actor,
        Execute,
        [arg!(id, String), arg!(node_id, String)]
    ),
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
                Kind::Boolean => json!({"type":"boolean"}),
                Kind::Integer => json!({"type":"integer"}),
                Kind::Count => json!({"type":"integer","minimum":0,"maximum":ValidationCount::MAX}),
                Kind::Map if !argument.required => {
                    json!({"type":["object","null"],"additionalProperties":{"type":"string"}})
                }
                Kind::Map => json!({"type":"object","additionalProperties":{"type":"string"}}),
                Kind::List if argument.required => {
                    json!({"type":"array","items":{"type":"string"},"minItems":1})
                }
                Kind::List => json!({"type":"array","items":{"type":"string"}}),
                Kind::Upload => json!({
                    "type":"string",
                    "description":"Raw CAS reference (CID) of the uploaded bytes; store them first with POST /v1/cas as application/octet-stream."
                }),
            };
            if let Some(default) = argument.default {
                schema["default"] = serde_json::from_str(default).expect("verb default is JSON");
            }
            properties.insert(argument.name.into(), schema);
            if argument.required {
                required.push(argument.name);
            }
        }
        let mut schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        if self.name == "update_repair" {
            schema["properties"]["changes"] = json!({
                "type":"object",
                "description":"Repairs keyed by a definition name from the update snapshot, or an unnamed failed definition hash. Submit all mutually dependent source fixes together.",
                "additionalProperties": {
                    "type":"object", "required":["source"], "additionalProperties":false,
                    "properties": {
                        "source":{"type":"string","description":"Complete Rust source or source bundle."},
                        "deps":{"type":["object","null"],"additionalProperties":{"type":"string"}},
                        "allowed_effects":{"type":["array","null"],"items":{"type":"string"}}
                    }
                }
            });
        }
        if self.name == "view" {
            schema["oneOf"] = json!([
                {"required":["target"],"not":{"anyOf":[
                    {"required":["actor"]},{"required":["table"]},{"required":["template"]},{"required":["order_by"]}
                ]}},
                {"required":["actor","table","template","order_by"],"not":{"required":["target"]}}
            ]);
        }
        schema
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
                Kind::Boolean => value.is_boolean(),
                Kind::Integer => value.as_i64().is_some(),
                Kind::Count => value
                    .as_u64()
                    .is_some_and(|value| ValidationCount::try_from(value).is_ok()),
                Kind::Map => {
                    value
                        .as_object()
                        .is_some_and(|map| map.values().all(Value::is_string))
                        || (!argument.required && value.is_null())
                }
                Kind::List => value.as_array().is_some_and(|items| {
                    items.iter().all(Value::is_string) && (!argument.required || !items.is_empty())
                }),
                Kind::Upload => value.is_string(),
            };
            if !valid {
                return Err(format!("{} invalid argument {}", self.name, argument.name));
            }
        }
        if self.name == "view" {
            let definition =
                object.len() == 1 && object.get("target").is_some_and(Value::is_string);
            let actor = object.len() == 4
                && ["actor", "table", "template"]
                    .iter()
                    .all(|name| object.get(*name).is_some_and(Value::is_string))
                && object
                    .get("order_by")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().all(Value::is_string));
            if !definition && !actor {
                return Err(
                    "view requires either target or actor, table, template, order_by".into(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_flag_is_boolean_and_move_requires_a_target_node() {
        let actors = lookup("actors").unwrap();
        assert_eq!(actors.schema()["properties"]["cluster"]["type"], "boolean");
        actors.normalize(&mut json!({})).unwrap();
        actors.normalize(&mut json!({"cluster": true})).unwrap();
        assert!(actors.normalize(&mut json!({"cluster": "true"})).is_err());
        assert!(
            lookup("move")
                .unwrap()
                .normalize(&mut json!({"id": "a0actor"}))
                .is_err()
        );
        lookup("move")
            .unwrap()
            .normalize(&mut json!({"id": "a0actor", "node_id": "node2"}))
            .unwrap();
        lookup("nodes").unwrap().normalize(&mut json!({})).unwrap();
    }

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
    fn dependency_pins_are_string_maps_on_every_transport() {
        for name in ["add", "update"] {
            let verb = lookup(name).unwrap();
            assert_eq!(
                verb.schema()["properties"]["deps"],
                json!({"type":["object","null"],"additionalProperties":{"type":"string"}})
            );
            let mut args = match name {
                "add" => json!({"source":"pub fn main() {}","deps":{"util":"util"}}),
                _ => json!({"name":"a","source":"pub fn main() {}","deps":{"util":"util"}}),
            };
            verb.normalize(&mut args).unwrap();
            args["deps"] = Value::Null;
            verb.normalize(&mut args).unwrap();
            for invalid in [json!({"util":1}), json!(["util"]), json!("util")] {
                args["deps"] = invalid;
                assert!(verb.normalize(&mut args).is_err(), "{name} {}", args["deps"]);
            }
        }
    }

    #[test]
    fn export_takes_target_words_and_import_takes_an_uploaded_reference() {
        let export = lookup("export").unwrap();
        assert!(export.family == Family::Definition);
        assert_eq!(
            export.schema()["properties"]["targets"],
            json!({"type":"array","items":{"type":"string"},"minItems":1})
        );
        export
            .normalize(&mut json!({"targets":["greet","util"]}))
            .unwrap();
        for mut invalid in [json!({"targets":[]}), json!({"targets":"greet"}), json!({})] {
            assert!(export.normalize(&mut invalid).is_err(), "{invalid}");
        }
        let import = lookup("import").unwrap();
        assert!(matches!(import.permission, Permission::Define));
        assert_eq!(import.schema()["properties"]["bundle"]["type"], "string");
        assert_eq!(
            import.schema()["properties"]["into"]["type"],
            json!(["string", "null"])
        );
        import
            .normalize(&mut json!({"bundle":"bafk","into":"friend"}))
            .unwrap();
        import.normalize(&mut json!({"bundle":"bafk"})).unwrap();
        assert!(import.normalize(&mut json!({"into":"friend"})).is_err());
        assert!(import.normalize(&mut json!({"bundle":7})).is_err());
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
