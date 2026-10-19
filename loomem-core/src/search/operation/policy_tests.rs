use super::OperationPolicy;

#[test]
fn policies_register_exact_ablation_matrix() {
    assert_eq!(
        serde_json::to_string(&OperationPolicy::E4).unwrap(),
        "\"E4\""
    );
    assert!(!OperationPolicy::E4.date_enabled());
    assert!(!OperationPolicy::E4.facets_enabled());
    assert!(!OperationPolicy::E4.is_e2());
    assert_eq!(
        serde_json::to_string(&OperationPolicy::E5).unwrap(),
        "\"E5\""
    );
    assert_eq!(
        serde_json::from_str::<OperationPolicy>("\"E5\"").unwrap(),
        OperationPolicy::E5
    );
    assert!(!OperationPolicy::E5.date_enabled());
    assert!(!OperationPolicy::E5.facets_enabled());
    assert!(!OperationPolicy::E5.is_e2());
    assert_eq!(OperationPolicy::default(), OperationPolicy::E0);
    assert_eq!(
        serde_json::to_string(&OperationPolicy::E0).unwrap(),
        "\"E0\""
    );
    assert!(OperationPolicy::E1D.date_enabled());
    assert!(!OperationPolicy::E1D.facets_enabled());
    assert!(OperationPolicy::E1F.facets_enabled());
    assert!(!OperationPolicy::E1F.date_enabled());
    assert!(OperationPolicy::E1C.date_enabled());
    assert!(OperationPolicy::E1C.facets_enabled());
}

#[test]
fn e6_is_opt_in_without_date_or_facet_retrieval() {
    assert_eq!(
        serde_json::to_string(&OperationPolicy::E6).unwrap(),
        "\"E6\""
    );
    assert_eq!(
        serde_json::from_str::<OperationPolicy>("\"E6\"").unwrap(),
        OperationPolicy::E6
    );
    assert!(!OperationPolicy::E6.date_enabled());
    assert!(!OperationPolicy::E6.facets_enabled());
    assert!(!OperationPolicy::E6.is_e2());
}
