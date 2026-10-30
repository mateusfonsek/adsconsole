use adsconsole_meta_ads::fields::{FieldKind, ObjectKind, find, normalize, to_param};
use serde_json::json;

#[test]
fn table_has_the_planned_fields() {
    for (name, kind) in [
        ("name", ObjectKind::Campaign),
        ("status", ObjectKind::Ad),
        ("daily_budget", ObjectKind::Campaign),
        ("daily_budget", ObjectKind::AdSet),
        ("lifetime_budget", ObjectKind::AdSet),
        ("bid_strategy", ObjectKind::Campaign),
        ("bid_amount", ObjectKind::AdSet),
        ("spend_cap", ObjectKind::Campaign),
        ("targeting", ObjectKind::AdSet),
        ("start_time", ObjectKind::AdSet),
        ("end_time", ObjectKind::AdSet),
        ("creative", ObjectKind::Ad),
    ] {
        assert!(find(name, kind).is_some(), "{name} em {kind:?}");
    }
    assert!(find("daily_budget", ObjectKind::Ad).is_none());
    assert!(find("targeting", ObjectKind::Campaign).is_none());
    assert_eq!(
        find("creative", ObjectKind::Ad).unwrap().read,
        "creative{id}"
    );
    assert_eq!(
        find("daily_budget", ObjectKind::AdSet).unwrap().value,
        FieldKind::Minor
    );
}

#[test]
fn normalize_and_back() {
    let budget = find("daily_budget", ObjectKind::AdSet).unwrap();
    assert_eq!(
        normalize(budget, &json!({"daily_budget": "2540"}), 2),
        json!(25.4)
    );
    assert_eq!(to_param(budget, &json!(25.4), 2).unwrap(), json!("2540"));
    assert_eq!(
        normalize(budget, &json!({"daily_budget": "1000"}), 0),
        json!(1000)
    );
    assert_eq!(to_param(budget, &json!(1000), 0).unwrap(), json!("1000"));
    assert!(to_param(budget, &json!(10.5), 0).is_err());
    assert!(to_param(budget, &json!("x"), 2).is_err());
    assert_eq!(normalize(budget, &json!({}), 2), json!(null));

    let creative = find("creative", ObjectKind::Ad).unwrap();
    assert_eq!(
        normalize(creative, &json!({"creative": {"id": "9"}}), 2),
        json!("9")
    );
    assert_eq!(
        to_param(creative, &json!("9"), 2).unwrap(),
        json!({"creative_id": "9"})
    );
    assert_eq!(normalize(creative, &json!({}), 2), json!(null));

    let status = find("status", ObjectKind::Campaign).unwrap();
    assert_eq!(
        normalize(status, &json!({"status": "PAUSED"}), 2),
        json!("PAUSED")
    );
    let targeting = find("targeting", ObjectKind::AdSet).unwrap();
    let t = json!({"geo_locations": {"countries": ["BR"]}});
    assert_eq!(normalize(targeting, &json!({"targeting": t}), 2), t);
    assert_eq!(to_param(targeting, &t, 2).unwrap(), t);
}
