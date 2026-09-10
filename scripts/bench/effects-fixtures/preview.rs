use loom::abilities::fs;
#[loom::def(effects=["fs.read","fs.read_optional","fs.write","cas.put","cas.get"])]
pub fn main(machine:String, existing:String, new_path:String, mode:String)->loom::Value {
    if mode=="seed" {
        fs::write(&machine,&existing,"before").expect("native write");
        return loom::serde_json::json!({"existing":fs::read(&machine,&existing).expect("native read"),"new":fs::read_optional(&machine,&new_path).expect("new read")});
    }
    if mode=="disk" {return loom::serde_json::json!({"existing":fs::read(&machine,&existing).expect("disk read"),"new":fs::read_optional(&machine,&new_path).expect("disk new")});}
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
        let before=change.before.as_ref().map(|hash|loom::perform::<String>(loom::Desc::new("cas.get",loom::serde_json::json!({"hash":hash}))).expect("before CAS"));
        let after=change.after.as_ref().map(|hash|loom::perform::<String>(loom::Desc::new("cas.get",loom::serde_json::json!({"hash":hash}))).expect("after CAS"));
        loom::serde_json::json!({"path":change.path,"before":before,"after":after})
    }).collect();
    loom::serde_json::json!({"preview":preview,"decoded":decoded})
}
