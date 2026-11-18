use super::{GENERIC_KEYS, ProductKey};

fn generic(edition_id: &str, build: u32) -> Option<String> {
    ProductKey::generic_for(edition_id, Some(build)).map(|key| key.expose().to_owned())
}

#[skuld::test]
fn every_generic_key_in_the_table_parses() {
    for row in GENERIC_KEYS {
        assert!(
            ProductKey::parse(row.key).is_some(),
            "the key of {} is not a product key",
            row.edition_id
        );
    }
}

#[skuld::test]
fn no_image_matches_two_rows() {
    for row in GENERIC_KEYS {
        for &build in row.builds.iter().flat_map(|(first, last)| [first, last]) {
            let matching = GENERIC_KEYS
                .iter()
                .filter(|other| other.matches(row.edition_id, build))
                .count();
            assert_eq!(matching, 1, "{} build {build}", row.edition_id);
        }
    }
}

#[skuld::test]
fn an_edition_has_the_key_of_its_edition_and_version_on_microsofts_page() {
    for (edition_id, build, key) in [
        ("IoTEnterpriseS", 26100, "KBN8V-HFGQ4-MGXVD-347P6-PDQGT"),
        ("IoTEnterpriseS", 19044, "KBN8V-HFGQ4-MGXVD-347P6-PDQGT"),
        ("EnterpriseS", 26100, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
        ("EnterpriseS", 19041, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
        ("EnterpriseS", 19044, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
        ("EnterpriseS", 19045, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
        ("EnterpriseSN", 19041, "92NFX-8DJQP-P6BBQ-THF9C-7CG2H"),
        ("IoTEnterpriseS", 19041, "KBN8V-HFGQ4-MGXVD-347P6-PDQGT"),
        ("EnterpriseS", 17763, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
        ("EnterpriseSN", 26100, "92NFX-8DJQP-P6BBQ-THF9C-7CG2H"),
        ("EnterpriseS", 14393, "DCPHK-NFMTC-H88MJ-PFHPY-QJ4BJ"),
        ("EnterpriseSN", 14393, "QFFDN-GRT3P-VKWWX-X7T3R-8B639"),
        ("EnterpriseS", 10240, "WNMTR-4C88C-JK8YV-HQ7T2-76DF9"),
        ("EnterpriseSN", 10240, "2F77B-TNFGY-69QQF-B8YKP-D69TJ"),
        ("Professional", 10240, "W269N-WFGWX-YVC9B-4J6C9-T83GX"),
        ("Professional", 22631, "W269N-WFGWX-YVC9B-4J6C9-T83GX"),
        ("Enterprise", 26200, "NPPR9-FWDCX-D2C8J-H872K-2YT43"),
        // A client release newer than any the table was copied at has its edition's key.
        ("Enterprise", 30000, "NPPR9-FWDCX-D2C8J-H872K-2YT43"),
        ("ServerStandard", 14393, "WC2BQ-8NRM3-FDDYY-2BFGV-KHKQY"),
        ("ServerDatacenter", 17763, "WMDGN-G9PQG-XVVXX-R3X43-63DFG"),
        ("ServerStandard", 20348, "VDYBN-27WPP-V4HQT-9VMD4-VMK7H"),
        ("ServerDatacenter", 26100, "D764K-2NDRG-47T6Q-P8T8W-YP6DF"),
    ] {
        assert_eq!(
            generic(edition_id, build).as_deref(),
            Some(key),
            "{edition_id} build {build}"
        );
    }
}

#[skuld::test]
fn an_edition_or_a_build_the_table_does_not_have_has_no_key() {
    // Not on the page: the subscription LTSC edition, Home, multi-session, an evaluation edition, no edition id.
    for edition_id in [
        "IoTEnterpriseSK",
        "Core",
        "ServerRdsh",
        "EnterpriseEval",
        "IoTEnterpriseSEval",
        "",
    ] {
        assert_eq!(generic(edition_id, 26100), None, "{edition_id}");
    }
    // A build the edition has no key for: an LTSC release that is not on the page, a build between LTSC 2021's family and
    // 2024, a client older than Windows 10, a server build the page does not name.
    assert_eq!(generic("EnterpriseS", 28000), None);
    assert_eq!(generic("EnterpriseS", 22621), None);
    assert_eq!(generic("EnterpriseS", 19040), None);
    assert_eq!(generic("EnterpriseS", 19046), None);
    assert_eq!(generic("Professional", 9600), None);
    assert_eq!(generic("ServerStandard", 22000), None);
    // The image names no build.
    assert!(ProductKey::generic_for("EnterpriseS", None).is_none());
}

#[skuld::test]
fn an_edition_id_is_matched_in_any_case() {
    assert_eq!(generic("iotenterprises", 26100), generic("IoTEnterpriseS", 26100));
    assert!(generic("iotenterprises", 26100).is_some());
}
