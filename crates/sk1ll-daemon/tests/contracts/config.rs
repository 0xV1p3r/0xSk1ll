use std::time::Duration;

#[test]
fn daemon_intervals_accept_documented_units_and_require_a_positive_value() {
    for (value, expected) in [
        ("250ms", Duration::from_millis(250)),
        ("90", Duration::from_secs(90)),
        ("90s", Duration::from_secs(90)),
        ("5m", Duration::from_secs(300)),
        ("1h", Duration::from_secs(3600)),
    ] {
        assert_eq!(sk1ll_daemon::parse_duration(value).unwrap(), expected);
    }
    for invalid in ["0s", "3w", "never"] {
        assert!(sk1ll_daemon::parse_duration(invalid).is_err(), "{invalid}");
    }
}
