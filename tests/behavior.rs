use digital_kvm::{
    config::{Config, usb_from_windows_path},
    controller::{Controller, Transition},
    ddc::{ioav_get_packet, parse_edid, parse_vcp_reply, set_packet},
};

fn windows_config() -> Config {
    serde_json::from_str(include_str!("../examples/windows.json")).unwrap()
}

#[test]
fn cli_accepts_options_before_or_after_the_command() {
    let executable = env!("CARGO_BIN_EXE_digital-kvm");
    let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/windows.json");
    for arguments in [
        vec!["set", "dp", "--force", "--dry-run", "--config"],
        vec!["--force", "--dry-run", "set", "dp", "--config"],
    ] {
        let result = std::process::Command::new(executable)
            .args(arguments)
            .arg(&config)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
            serde_json::json!({"would_select":"dp"})
        );
    }
    let result = std::process::Command::new(executable)
        .arg("--force")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("requires a command"));
}

#[test]
fn only_selected_switch_hub_matches_among_similar_devices() {
    let config = windows_config();
    config.validate().unwrap();
    let receiver = usb_from_windows_path(
        r"\\?\USB#VID_0B05&PID_1ACE#W1MPGDD00DC9#{a5dcbf10-6530-11d2-901f-00c04fb951ed}",
    )
    .unwrap();
    assert!(!config.usb.matches(&receiver));
    let mut another = receiver.clone();
    another.serial = "another-device".into();
    assert!(!config.usb.matches(&another));
    let hub = usb_from_windows_path(r"\\?\USB#VID_05E3&PID_0610#6&23491980&0&1#{guid}").unwrap();
    assert!(config.usb.matches(&hub));
    let other_hub =
        usb_from_windows_path(r"\\?\USB#VID_05E3&PID_0610#7&63b41e3&0&4#{guid}").unwrap();
    assert!(!config.usb.matches(&other_hub));
    assert!(usb_from_windows_path(r"\\?\HID#VID_0B05&PID_1ACE#something#{guid}").is_none());
}

#[test]
fn startup_absence_and_duplicate_snapshots_do_not_handoff() {
    let mut state = Controller::new(false);
    assert_eq!(state.observe(false), None);
    assert_eq!(state.observe(true), Some(Transition::ClaimLocal));
    assert_eq!(state.observe(true), None);
    assert_eq!(state.observe(false), Some(Transition::HandoffRemote));
    assert_eq!(state.observe(false), None);
}

#[test]
fn coalesced_bounce_stays_on_final_host() {
    // The native loop presents only the final snapshot after the debounce
    // interval: disconnect/reconnect inside that interval causes no handoff.
    let mut state = Controller::new(true);
    assert_eq!(state.observe(true), None);
    assert_eq!(state.observe(false), Some(Transition::HandoffRemote));
    assert_eq!(state.observe(true), Some(Transition::ClaimLocal));
}

#[test]
fn sleep_teardown_does_not_switch_to_other_host() {
    let mut state = Controller::new(true);
    state.suspend();
    assert_eq!(state.observe(false), None);
    assert_eq!(state.resume(false), None);
    assert_eq!(state.observe(false), None);
    assert_eq!(state.observe(true), Some(Transition::ClaimLocal));
    state.suspend();
    assert_eq!(state.resume(true), Some(Transition::ClaimLocal));
}

#[test]
fn windows_and_macos_profiles_select_complementary_inputs() {
    let pc = windows_config();
    let mut mac: Config = serde_json::from_str(include_str!("../examples/macos.json")).unwrap();
    assert!(mac.validate().is_err());
    mac.usb.instance_contains = Some("IOService:/learned-switch-hub".into());
    mac.validate().unwrap();
    assert_eq!(pc.local_input, mac.remote_input);
    assert_eq!(pc.remote_input, mac.local_input);
    assert_eq!(pc.monitors[0].inputs, mac.monitors[0].inputs);
}

#[test]
fn malformed_configuration_cannot_arm_monitor_commands() {
    let mut config = windows_config();
    config.local_input = config.remote_input.clone();
    assert!(config.validate().is_err());
    let mut config = windows_config();
    config.monitors[0].inputs.remove("usbc");
    assert!(config.validate().is_err());
    let mut config = windows_config();
    config.arrival_attempts = 1000;
    assert!(config.validate().is_err());
    let mut config = windows_config();
    config.usb.serial = Some(String::new());
    assert!(config.validate().is_err());
    let mut text = serde_json::to_value(windows_config()).unwrap();
    text["debouce_ms"] = serde_json::json!(100);
    assert!(serde_json::from_value::<Config>(text).is_err());
}

#[test]
fn lg_and_standard_wire_packets_have_correct_source_and_checksum() {
    assert_eq!(
        set_packet(0x50, 0xf4, 0xd0),
        [0x50, 0x84, 0x03, 0xf4, 0, 0xd0, 0x9d]
    );
    assert_eq!(
        set_packet(0x50, 0xf4, 0xd1),
        [0x50, 0x84, 0x03, 0xf4, 0, 0xd1, 0x9c]
    );
    for packet in [set_packet(0x51, 0x60, 0x0f), set_packet(0x50, 0xf4, 0xd1)] {
        assert_eq!(packet.iter().fold(0x6eu8, |a, b| a ^ b), 0);
    }
}

#[test]
fn apple_ioav_reads_use_transport_specific_checksum() {
    assert_eq!(ioav_get_packet(0x10), [0x82, 1, 0x10, 0xfd]);
    assert_eq!(ioav_get_packet(0x60), [0x82, 1, 0x60, 0x8d]);
}

#[test]
fn input_readback_rejects_bad_checksum_wrong_feature_and_error_replies() {
    let dp_reply = [0x6e, 0x88, 2, 0, 0x60, 0, 0, 0x12, 0, 0x0f, 0xc9];
    assert_eq!(parse_vcp_reply(&dp_reply, 0x60), Some(15));
    assert_eq!(parse_vcp_reply(&dp_reply, 0x10), None);
    let mut corrupt = dp_reply;
    corrupt[9] ^= 1;
    assert_eq!(parse_vcp_reply(&corrupt, 0x60), None);
    let mut unsupported = dp_reply;
    unsupported[3] = 1;
    unsupported[10] ^= 1;
    assert_eq!(parse_vcp_reply(&unsupported, 0x60), None);
    assert_eq!(parse_vcp_reply(&dp_reply[..10], 0x60), None);
}

#[test]
fn edid_identity_rejects_other_monitor_and_corrupt_data() {
    let hex = "00FFFFFFFFFFFF001E6DB9C47747090002240104B5793378F93AA5AE4E3EAA250A5054210900D1C06140454081C001010101010101014ED470A0D0A0465030203A00BAFE4100001A000000FD0C30F0FFFFFF010A202020202020000000FC004C4720554C545241474541522B000000FF003630324E54565348573131390A020B";
    let mut bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let identity = parse_edid(&bytes, "test".into()).unwrap();
    assert_eq!(identity.manufacturer, "GSM");
    assert_eq!(identity.product_id, 50361);
    assert_eq!(identity.serial, "602NTVSHW119");
    let config = windows_config();
    assert!(identity.matches(&config.monitors[0]));
    let mut wrong = config.monitors[0].clone();
    wrong.serial = Some("different-monitor".into());
    assert!(!identity.matches(&wrong));
    bytes[10] ^= 1;
    assert!(parse_edid(&bytes, "test".into()).is_err());
    assert!(parse_edid(&bytes[..50], "test".into()).is_err());
}
