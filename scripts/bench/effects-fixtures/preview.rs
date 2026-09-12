use loom::fs;
pub fn main(machine:String, existing:String, new_path:String, mode:String)->loom::Value {
    if mode=="seed" {
        fs::write(&machine,&existing,"before").expect("native write");
        return { let mut map=loom::serde_json::Map::new(); map.insert("existing".into(), loom::serde_json::to_value(fs::read(&machine,&existing).expect("native read")).expect("encode value"));map.insert("new".into(), loom::serde_json::to_value(fs::read_optional(&machine,&new_path).expect("new read")).expect("encode value")); loom::Value::Object(map) };
    }
    if mode=="disk" {return { let mut map=loom::serde_json::Map::new(); map.insert("existing".into(), loom::serde_json::to_value(fs::read(&machine,&existing).expect("disk read")).expect("encode value"));map.insert("new".into(), loom::serde_json::to_value(fs::read_optional(&machine,&new_path).expect("disk new")).expect("encode value")); loom::Value::Object(map) };}
    let preview=loom::preview::writes(|| {
        if mode=="noop" {fs::write(&machine,&existing,"before").expect("noop write");}
        else {
            fs::write(&machine,&existing,"intermediate").expect("first preview write");
            fs::write(&machine,&existing,"after").expect("last preview write");
            fs::write(&machine,&new_path,"created").expect("new preview write");
        }
        fs::read(&machine,&existing).expect("overlay read")
    }).expect("preview");
    let decoded:Vec<loom::Value>=preview.filesystem_changes.iter().map(|change| {
        let before=change.before.as_ref().map(|hash|loom::perform::<String>("cas.get",{ let mut map=loom::serde_json::Map::new(); map.insert("hash".into(), loom::serde_json::to_value(hash).expect("encode value")); loom::Value::Object(map) }).expect("before CAS"));
        let after=change.after.as_ref().map(|hash|loom::perform::<String>("cas.get",{ let mut map=loom::serde_json::Map::new(); map.insert("hash".into(), loom::serde_json::to_value(hash).expect("encode value")); loom::Value::Object(map) }).expect("after CAS"));
        { let mut map=loom::serde_json::Map::new(); map.insert("path".into(), loom::serde_json::to_value(&change.path).expect("encode value"));map.insert("before".into(), loom::serde_json::to_value(before).expect("encode value"));map.insert("after".into(), loom::serde_json::to_value(after).expect("encode value")); loom::Value::Object(map) }
    }).collect();
    { let mut map=loom::serde_json::Map::new(); map.insert("preview".into(), loom::serde_json::to_value(preview).expect("encode value"));map.insert("decoded".into(), loom::serde_json::to_value(decoded).expect("encode value")); loom::Value::Object(map) }
}
