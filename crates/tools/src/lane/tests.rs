use super::*;

#[test]
fn classify_first_gate_tools() {
    assert_eq!(classify("read"), Lane::Local);
    assert_eq!(classify("write"), Lane::Local);
    assert_eq!(classify("edit"), Lane::Local);
    assert_eq!(classify("grep"), Lane::Local);
    assert_eq!(classify("glob"), Lane::Local);
    assert_eq!(classify("bash"), Lane::Net);
}

#[test]
fn unknown_tools_default_to_heavy() {
    assert_eq!(classify("mysterious-new-tool"), Lane::Heavy);
    assert_eq!(classify(""), Lane::Heavy);
}

#[test]
fn lane_policy_local_numbers() {
    let p = LanePolicy::local();
    assert_eq!(p.lane, Lane::Local);
    assert_eq!(p.max_slots, 10);
    assert_eq!(p.default_timeout_secs, 30);
    assert_eq!(p.max_timeout_secs, 60);
    assert!(p.sandbox_net);
}

#[test]
fn lane_policy_net_numbers() {
    let p = LanePolicy::net();
    assert_eq!(p.lane, Lane::Net);
    assert_eq!(p.max_slots, 5);
    assert_eq!(p.default_timeout_secs, 60);
    assert_eq!(p.max_timeout_secs, 120);
    assert!(!p.sandbox_net);
}

#[test]
fn lane_policy_heavy_numbers() {
    let p = LanePolicy::heavy();
    assert_eq!(p.lane, Lane::Heavy);
    assert_eq!(p.max_slots, 1);
    assert_eq!(p.default_timeout_secs, 600);
    assert_eq!(p.max_timeout_secs, 1800);
    assert!(!p.sandbox_net);
}

#[test]
fn for_lane_round_trip() {
    for lane in [Lane::Local, Lane::Net, Lane::Heavy] {
        assert_eq!(LanePolicy::for_lane(lane).lane, lane);
    }
}
