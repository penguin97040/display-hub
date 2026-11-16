//! The reference display draws each shape, and the "can't show this" card, in its text screen.

use std::path::Path;

use display_simulator::render::{count_pages, screen};
use display_simulator::{Device, NoJitter};
use hub_protocol::receive;

fn drawn(example: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1/examples");
    let snap = receive(&std::fs::read(path.join(example)).unwrap()).unwrap();
    let now = snap.generated_at;
    let mut d = Device::new(None, Box::new(NoJitter));
    d.accept(now, Ok(snap));
    screen(&d, now)
}

#[test]
fn every_shape_is_drawn() {
    let s = drawn("valid/all-shapes.json");
    assert_eq!(count_pages(&s), 5);
    assert!(s.contains("1234.5 k"), "{s}");
    assert!(s.contains("NZ$"));
    assert!(s.contains("    - NZ$"), "a null figure is a dash, not zero: {s}");
    assert!(s.contains("- display-hub · passed"));
    assert!(s.contains("The monthly allowance resets on the first."));
    assert!(s.contains("unknown"));
    assert!(!s.contains("can't show"));
}

#[test]
fn a_page_that_cant_be_shown_is_a_card_and_the_others_still_show() {
    let s = drawn("invalid/unknown-kind.json");
    assert_eq!(count_pages(&s), 2);
    assert!(s.contains("This display can't show this page."));
    assert!(s.contains("It may need a firmware update."));
    let s = drawn("invalid/shape-bad-data.json");
    assert!(s.contains("This display can't show this page."));
    assert!(
        !s.contains("firmware update"),
        "only an unknown kind suggests an update"
    );
    assert!(s.contains("Tokens today"));
    let s = drawn("valid/custom-kind.json");
    assert!(
        s.contains("It may need a firmware update."),
        "this display knows no custom kinds"
    );
}
