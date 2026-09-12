use dw_host_seams::{OptionKind, TypedOption, typed_input};
use serde_json::json;
fn main() {
    let options = [TypedOption {
        name: "name".into(),
        required: true,
        kind: OptionKind::String {
            max_length: 80,
            choices: vec![],
        },
    }];
    let input = typed_input(&options, &json!({"name":"Pebble"})).unwrap();
    println!(
        "{}",
        json!({"prototype":true,"operation":"lookup","input":input})
    );
}
