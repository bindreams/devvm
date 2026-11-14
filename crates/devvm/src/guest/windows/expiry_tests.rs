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
// activated the evaluation, 1/129600 a minute later): what the LTSC evaluation reads after a successful activation is not
// measured.
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
fn an_unlicensed_evaluation_is_unavailable_and_names_its_status() {
    let error = expiry_from(LTSC_EVAL, true, now()).expect_err("an unlicensed evaluation has no expiry");
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(
        error.message(),
        "Windows did not license the evaluation: no activation was tried; status 5"
    );
}

// What the first-logon script sends after `Activate`: flat fields, null when absent; the codes are synthesized, as no
// failed activation is captured.
const ACTIVATION_SUCCEEDED: &str = r#"DEVVM-LICENSE={"LicenseStatus":5,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"IoTEnterpriseSEval","ActivationOutcome":"succeeded","ActivationMessage":null,"ActivationNativeErrorCode":null,"ActivationStatusCode":null,"ActivationReturnValue":null}"#;
const ACTIVATION_FAILED: &str = r#"DEVVM-LICENSE={"LicenseStatus":5,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"IoTEnterpriseSEval","ActivationOutcome":"failed","ActivationMessage":"The Software Licensing Service reported that the computer could not be activated. 0xC004F074","ActivationNativeErrorCode":"Failed","ActivationStatusCode":1,"ActivationReturnValue":null}"#;
const ACTIVATION_RETURNED: &str = r#"DEVVM-LICENSE={"LicenseStatus":5,"GracePeriodRemaining":0,"Description":"d","LicenseFamily":"IoTEnterpriseSEval","ActivationOutcome":"failed","ActivationMessage":"Activate returned 3221549172","ActivationNativeErrorCode":null,"ActivationStatusCode":null,"ActivationReturnValue":3221549172}"#;

#[skuld::test]
fn an_unlicensed_evaluation_names_the_code_the_guest_sent() {
    let error = expiry_from(ACTIVATION_FAILED, true, now()).expect_err("an unlicensed evaluation has no expiry");
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(
        error.message(),
        "Windows did not license the evaluation: activation failed: The Software Licensing Service reported that the computer could not be activated. 0xC004F074 (native error code Failed, status code 1); status 5"
    );
}

#[skuld::test]
fn an_unlicensed_evaluation_names_the_return_value_of_an_activation_that_returned_nonzero() {
    let error = expiry_from(ACTIVATION_RETURNED, true, now()).expect_err("an unlicensed evaluation has no expiry");
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(
        error.message(),
        "Windows did not license the evaluation: activation failed: Activate returned 3221549172 (return value 3221549172); status 5"
    );
}

#[skuld::test]
fn an_evaluation_that_stays_unlicensed_after_an_activation_is_unavailable_whatever_the_activation_said() {
    let error = expiry_from(ACTIVATION_SUCCEEDED, true, now()).expect_err("an unlicensed evaluation has no expiry");
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(
        error.message(),
        "Windows did not license the evaluation: activation succeeded; status 5"
    );
}

#[skuld::test]
fn a_licensed_read_is_not_held_back_by_an_activation_field() {
    let read = r#"DEVVM-LICENSE={"LicenseStatus":1,"GracePeriodRemaining":129600,"Description":"d","LicenseFamily":"e","ActivationOutcome":"succeeded"}"#;
    assert_eq!(
        expiry_from(read, true, now()).unwrap(),
        Some(now() + Duration::from_secs(129_600 * 60))
    );
}

#[skuld::test]
fn a_license_is_described_in_plain_words() {
    assert_eq!(describe(EN_US), "license status 1, 129600 minutes of grace");
    assert_eq!(
        describe(ACTIVATION_SUCCEEDED),
        "license status 5, 0 minutes of grace, activation succeeded"
    );
    assert_eq!(
        describe(ACTIVATION_FAILED),
        "license status 5, 0 minutes of grace, activation failed: The Software Licensing Service reported that the computer could not be activated. 0xC004F074 (native error code Failed, status code 1)"
    );
    assert_eq!(describe(NO_PRODUCT), "no licensed product");
}

#[skuld::test]
fn a_license_that_cannot_be_read_is_described_with_the_reason_and_not_the_line() {
    let described = describe("DEVVM-LICENSE=not json");
    assert!(
        described.starts_with("cannot read the guest's DEVVM-LICENSE= line ("),
        "{described}"
    );
    assert!(
        !described.contains("not json"),
        "the build error carries the line: {described}"
    );
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
    let error = expiry_from(read, true, now()).expect_err("a licensed evaluation with no grace has no expiry");
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().ends_with("; status 1"), "{error}");
}

#[skuld::test]
fn a_non_evaluation_image_with_grace_left_ends_with_it() {
    // The image decides, not the read: a licensed non-evaluation one with grace left still reports its own end.
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
        // The activation's outcome is text: a nested object of an earlier script is not read.
        r#"DEVVM-LICENSE={"LicenseStatus":1,"GracePeriodRemaining":5,"ActivationOutcome":{"Outcome":"succeeded"}}"#,
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
