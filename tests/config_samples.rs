use dfor77::Config;

#[test]
fn bundled_config_samples_parse_with_safe_defaults() {
    let sample =
        Config::from_toml_str_with_env(include_str!("../config/dfor77.sample.toml"), |name| {
            match name {
                "DISCORD_BOT_TOKEN" => Some("sample-token".into()),
                "DFOR77_AUDIT_HASH_KEY" => Some("sample-audit-key".into()),
                _ => None,
            }
        })
        .unwrap();

    assert!(sample.gateway_live);
    assert_eq!(sample.audit_content_mode, "hash");
    assert_eq!(sample.audit_hash_key_env, "DFOR77_AUDIT_HASH_KEY");
    assert_eq!(sample.rest_base_url, "https://discord.com");
    assert!(sample.snapshot_event_queue_capacity >= 4096);
    assert!(sample.snapshot_message_ttl_ms >= 86400000);
}
