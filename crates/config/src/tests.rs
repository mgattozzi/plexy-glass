use super::*;

#[test]
fn config_default_works() {
    let c = Config::default();
    assert!(c.status.left.is_empty());
}
