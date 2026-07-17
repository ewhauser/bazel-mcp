#[test]
fn invoice_total_includes_service_fee() {
    let actual = 41;
    assert_eq!(actual, 42, "invoice total should include the service fee");
}
