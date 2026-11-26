use super::session::parse;

#[test]
fn a_timeline_is_asked_for_by_seconds_frames_and_a_file() {
    assert_eq!(parse("120,20,D:/a.json"), (Some((vec![120.0], 20)), "D:/a.json"));
    assert_eq!(parse("120+130.5,20,D:/a,b.json"), (Some((vec![120.0, 130.5], 20)), "D:/a,b.json"));
    assert_eq!(parse("D:/a,1,2.json"), (None, "D:/a,1,2.json"));
    assert_eq!(parse("D:/a.json"), (None, "D:/a.json"));
}
