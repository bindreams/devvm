use std::time::{Duration, SystemTime};

use super::{describe, expiry_from};
use crate::ErrorKind;

// Captured serial lines. The product's marker is `DEVVM-LICENSE`; the rest of each line is as captured.
const EN_US: &str = r#"DEVVM-LICENSE={"Name":"Windows(R), EnterpriseEval edition","Description":"Windows(R) Operating System, TIMEBASED_EVAL channel","LicenseStatus":1,"GracePeriodRemaining":129600,"LicenseFamily":"EnterpriseEval","ProductKeyChannel":"Retail:TB:Eval","PartialProductKey":"YG9R6"}"#;
// The de-DE install's capture: Windows reports these WMI strings in English whatever the language.
const DE_DE: &str = r#"DEVVM-LICENSE={"Name":"Windows(R), EnterpriseEval edition","Description":"Windows(R) Operating System, TIMEBASED_EVAL channel","LicenseStatus":1,"GracePeriodRemaining":129600,"LicenseFamily":"EnterpriseEval","ProductKeyChannel":"Retail:TB:Eval","PartialProductKey":"YG9R6"}"#;
// Synthesized: Name and Description in German with non-ASCII letters, escaped as the first-logon script escapes them.
const LOCALIZED_TEXT: &str = r#"DEVVM-LICENSE={"Name":"Windows(R), Enterprise-Testversion","Description":"Windows(R)-Betriebssystem, TIMEBASED_EVAL-Kanal f\u00fcr Testversionen","LicenseStatus":1,"GracePeriodRemaining":129600,"LicenseFamily":"EnterpriseEval","ProductKeyChannel":"Retail:TB:Eval","PartialProductKey":"YG9R6"}"#;
// Measured without the activation the bootstrap now makes first (vm-windows run 43545384836 read 5/0 before Windows
// activated the evaluation, 1/129600 a minute later): whether the LTSC evaluation stays unlicensed after a successful
// activation is not measured.
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
fn a_localized_evaluation_guest_expires_grace_minutes_from_now() {
    // Absolute: a read compared with another read could not fail when both are wrong the same way.
    for read in [DE_DE, LOCALIZED_TEXT] {
        assert_eq!(
            expiry_from(read, true, now()).unwrap(),
            Some(now() + Duration::from_secs(129_600 * 60)),
            "{read}"
        );
    }
}

#[skuld::test]
fn an_evaluation_guest_that_is_not_licensed_and_made_no_activation_is_expired_now() {
    assert_eq!(expiry_from(LTSC_EVAL, true, now()).unwrap(), Some(now()));
}

// What the first-logon script sends after `Activate`: an evaluation that stays unlicensed after a successful activation
// is born expired.
const ACTIVATED_STILL_UNLICENSED: &str = r#"DEVVM-LICENSE={"LicenseStatus":5,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"IoTEnterpriseSEval","Activation":"activated"}"#;
const ACTIVATION_FAILED: &str = r#"DEVVM-LICENSE={"LicenseStatus":5,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"IoTEnterpriseSEval","Activation":"failed: The Software Licensing Service reported that the computer could not be activated. 0xC004F074"}"#;

#[skuld::test]
fn an_evaluation_that_stays_unlicensed_after_a_successful_activation_is_born_expired() {
    assert_eq!(
        expiry_from(ACTIVATED_STILL_UNLICENSED, true, now()).unwrap(),
        Some(now())
    );
}

#[skuld::test]
fn a_failed_activation_of_an_evaluation_is_unavailable_and_names_the_failure_and_the_network() {
    let error = expiry_from(ACTIVATION_FAILED, true, now()).expect_err("a failed activation is not an expiry");
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let failure = "The Software Licensing Service reported that the computer could not be activated. 0xC004F074";
    assert_eq!(
        error.message(),
        format!(
            "Windows could not activate the evaluation ({failure}); the build needs the network, and a later build tries again"
        )
    );
}

#[skuld::test]
fn a_licensed_read_is_not_held_back_by_an_activation_field() {
    let read = r#"DEVVM-LICENSE={"LicenseStatus":1,"GracePeriodRemaining":129600,"Description":"d","LicenseFamily":"e","Activation":"activated"}"#;
    assert_eq!(
        expiry_from(read, true, now()).unwrap(),
        Some(now() + Duration::from_secs(129_600 * 60))
    );
}

#[skuld::test]
fn an_activation_that_is_neither_activated_nor_failed_is_an_error() {
    let read = r#"DEVVM-LICENSE={"LicenseStatus":5,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"e","Activation":"maybe"}"#;
    let error = expiry_from(read, true, now()).expect_err(read);
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("maybe"), "{error}");
}

#[skuld::test]
fn a_license_is_described_in_plain_words() {
    assert_eq!(describe(EN_US), "license status 1, 129600 minutes of grace");
    assert_eq!(
        describe(ACTIVATED_STILL_UNLICENSED),
        "license status 5, 0 minutes of grace, activation activated"
    );
    assert_eq!(
        describe(ACTIVATION_FAILED),
        "license status 5, 0 minutes of grace, activation failed: The Software Licensing Service reported that the computer could not be activated. 0xC004F074"
    );
    assert_eq!(describe(NO_PRODUCT), "no licensed product");
    assert_eq!(describe("DEVVM-LICENSE=not json"), "license cannot be read");
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
