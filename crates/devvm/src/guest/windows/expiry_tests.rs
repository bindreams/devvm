use std::time::{Duration, SystemTime};

use super::expiry_from;
use crate::ErrorKind;

// Captured serial lines (`.tmp/claude/u5/fixtures/m9/`, run 43333774008). The spike's marker was `DEVVM-M9-LICENSE`; the
// product's is `DEVVM-LICENSE`, the rest of the line is as captured.
const EN_US: &str = r#"DEVVM-LICENSE={"Name":"Windows(R), EnterpriseEval edition","Description":"Windows(R) Operating System, TIMEBASED_EVAL channel","LicenseStatus":1,"GracePeriodRemaining":129600,"LicenseFamily":"EnterpriseEval","ProductKeyChannel":"Retail:TB:Eval","PartialProductKey":"YG9R6"}"#;
// The de-DE install prints the same text: every field that is read is a number or an identifier.
const DE_DE: &str = EN_US;
const LTSC_EVAL: &str = r#"DEVVM-LICENSE={"Name":"Windows(R), IoTEnterpriseSEval edition","Description":"Windows(R) Operating System, TIMEBASED_EVAL channel","LicenseStatus":5,"GracePeriodRemaining":0,"LicenseFamily":"IoTEnterpriseSEval","ProductKeyChannel":"Retail:TB:Eval","PartialProductKey":"M4K7D"} @639301258508628226"#;
// Synthesized from the documented values (LicenseStatus 5 is "Notification"; a volume edition without a key): no capture
// of an unactivated non-evaluation guest exists until the owner's LTSC first run.
const UNACTIVATED: &str = r#"DEVVM-LICENSE={"Description":"Windows(R) Operating System, RETAIL channel","LicenseStatus":5,"GracePeriodRemaining":0,"LicenseFamily":"IoTEnterpriseS"}"#;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

#[skuld::test]
fn an_evaluation_guest_expires_grace_minutes_from_now() {
    let expires = expiry_from(EN_US, true, now())
        .unwrap()
        .expect("an evaluation has an expiry");
    assert_eq!(expires, now() + Duration::from_secs(129_600 * 60));
}

#[skuld::test]
fn a_localized_evaluation_guest_expires_the_same_way() {
    assert_eq!(
        expiry_from(DE_DE, true, now()).unwrap(),
        expiry_from(EN_US, true, now()).unwrap()
    );
}

#[skuld::test]
fn an_evaluation_guest_that_is_not_licensed_is_born_expired() {
    assert_eq!(expiry_from(LTSC_EVAL, true, now()).unwrap(), Some(now()));
}

#[skuld::test]
fn an_unactivated_non_evaluation_guest_has_no_expiry() {
    assert_eq!(expiry_from(UNACTIVATED, false, now()).unwrap(), None);
}

#[skuld::test]
fn a_licensed_status_with_zero_grace_has_no_expiry_unless_the_image_is_an_evaluation() {
    let read =
        r#"DEVVM-LICENSE={"LicenseStatus":1,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"Enterprise"}"#;
    assert_eq!(expiry_from(read, false, now()).unwrap(), None);
    assert_eq!(expiry_from(read, true, now()).unwrap(), Some(now()));
}

#[skuld::test]
fn a_non_evaluation_image_with_grace_left_ends_with_it() {
    // The image decides, not the read: only an evaluation image is born expired, and a licensed non-evaluation one with
    // grace left still reports its own end.
    assert_eq!(
        expiry_from(EN_US, false, now()).unwrap(),
        Some(now() + Duration::from_secs(129_600 * 60))
    );
}

#[skuld::test]
fn an_unparseable_read_is_an_error() {
    for read in [
        "",
        "DEVVM-LICENSE=",
        "DEVVM-LICENSE=not json",
        r#"DEVVM-LICENSE={"LicenseStatus":1}"#,
        r#"DEVVM-LICENSE={"LicenseStatus":"1","GracePeriodRemaining":5}"#,
        r#"DEVVM-LICENSE={"LicenseStatus":1,"GracePeriodRemaining":-5}"#,
        r#"DEVVM-LICENSE={"LicenseStatus":1,"GracePeriodRemaining":18446744073709551615}"#,
        r#"DEVVM-OTHER={"LicenseStatus":1,"GracePeriodRemaining":5}"#,
        r#"{"LicenseStatus":1,"GracePeriodRemaining":5}"#,
    ] {
        for evaluation in [true, false] {
            let error = expiry_from(read, evaluation, now()).expect_err(read);
            assert_eq!(error.kind(), ErrorKind::Internal, "{read}");
            assert!(error.message().contains("DEVVM-LICENSE"), "{read}: {}", error.message());
        }
    }
}

// The first-logon script sends `null` when no product has a key.
const NO_PRODUCT: &str = "DEVVM-LICENSE=null";

#[skuld::test]
fn a_keyless_non_evaluation_install_has_no_expiry() {
    assert_eq!(expiry_from(NO_PRODUCT, false, now()).unwrap(), None);
}

#[skuld::test]
fn an_evaluation_edition_with_no_licensed_product_is_an_error() {
    let error = expiry_from(NO_PRODUCT, true, now()).expect_err("an evaluation must read as licensed or expired");
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert!(error.message().contains("DEVVM-LICENSE"), "{}", error.message());
}
