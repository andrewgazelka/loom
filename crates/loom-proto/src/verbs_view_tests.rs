use super::*;

#[test]
fn view_forms_are_exclusive_and_order_columns_are_strings() {
    let verb = lookup("view").unwrap();
    for mut valid in [json!({"target":"definition"}), json!({"actor":"a","table":"t","template":"h","order_by":["key"]})] {
        verb.normalize(&mut valid).unwrap();
    }
    for mut invalid in [
        json!({"target":"definition","actor":"a"}),
        json!({"actor":"a","table":"t","template":"h"}),
        json!({"actor":"a","table":"t","template":"h","order_by":[1]}),
        json!({"target":null}),
    ] {
        assert!(verb.normalize(&mut invalid).is_err());
    }
}

#[test]
fn ephemeral_spawn_and_subscriptions_share_the_registry() {
    let mut args = json!({"def":"counter-v1","durability":"ephemeral"});
    lookup("spawn").unwrap().normalize(&mut args).unwrap();
    assert_eq!(args["durability"], "ephemeral");
    lookup("subscriptions").unwrap().normalize(&mut json!({"id":"a"})).unwrap();
}
