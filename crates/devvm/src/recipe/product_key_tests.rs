use super::{GENERIC_KEYS, ProductKey};

fn generic_at(edition_id: &str, build: u32, spbuild: Option<u32>) -> Option<String> {
    ProductKey::generic_for(edition_id, Some(build), spbuild).map(|key| key.expose().to_owned())
}

/// The key of an image whose `SPBUILD` is the newest any row asks for.
fn generic(edition_id: &str, build: u32) -> Option<String> {
    generic_at(edition_id, build, Some(u32::MAX))
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
        for span in row.builds {
            for build in [span.first, span.last] {
                let matching = GENERIC_KEYS
                    .iter()
                    .filter(|other| other.matches(row.edition_id, build, Some(span.spbuild_from)))
                    .count();
                assert_eq!(matching, 1, "{} build {build}", row.edition_id);
            }
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
        ("EnterpriseS", 17763, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
        ("EnterpriseSN", 26100, "92NFX-8DJQP-P6BBQ-THF9C-7CG2H"),
        ("EnterpriseS", 14393, "DCPHK-NFMTC-H88MJ-PFHPY-QJ4BJ"),
        ("EnterpriseSN", 14393, "QFFDN-GRT3P-VKWWX-X7T3R-8B639"),
        ("EnterpriseS", 10240, "WNMTR-4C88C-JK8YV-HQ7T2-76DF9"),
        ("EnterpriseSN", 10240, "2F77B-TNFGY-69QQF-B8YKP-D69TJ"),
        ("Professional", 10240, "W269N-WFGWX-YVC9B-4J6C9-T83GX"),
        ("Professional", 22631, "W269N-WFGWX-YVC9B-4J6C9-T83GX"),
        ("Enterprise", 26200, "NPPR9-FWDCX-D2C8J-H872K-2YT43"),
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
    // A build the edition has no key for: an LTSC release that is not on the page, a build between LTSC 2021's family
    // and 2024, a client older than Windows 10, a server build the page does not name.
    assert_eq!(generic("EnterpriseS", 28000), None);
    assert_eq!(generic("EnterpriseS", 22621), None);
    assert_eq!(generic("Professional", 9600), None);
    assert_eq!(generic("ServerStandard", 22000), None);
    // The image names no build.
    assert!(ProductKey::generic_for("EnterpriseS", None, Some(1742)).is_none());
}

#[skuld::test]
fn a_range_of_builds_has_its_key_from_its_first_build_to_its_last() {
    for (edition_id, build, has_key) in [
        ("Professional", 10239, false),
        ("Professional", 10240, true),
        ("EnterpriseS", 19040, false),
        ("EnterpriseS", 19041, true),
        ("EnterpriseS", 19045, true),
        ("EnterpriseS", 19046, false),
        ("IoTEnterpriseS", 19043, false),
        ("IoTEnterpriseS", 19044, true),
        ("IoTEnterpriseS", 19045, false),
    ] {
        assert_eq!(
            generic(edition_id, build).is_some(),
            has_key,
            "{edition_id} build {build}"
        );
    }
}

#[skuld::test]
fn the_first_spbuild_of_a_gated_row_is_the_one_media_must_reach() {
    for (edition_id, build, first) in [
        ("IoTEnterpriseS", 19044, Some(2905)),
        ("IoTEnterpriseS", 26100, None),
        ("IoTEnterpriseS", 19045, None),
        ("EnterpriseS", 19044, None),
        ("Nothing", 19044, None),
    ] {
        assert_eq!(
            ProductKey::generic_first_spbuild(edition_id, build),
            first,
            "{edition_id} build {build}"
        );
    }
}

#[skuld::test]
fn iot_ltsc_2021_has_its_key_from_image_19044_2905_on() {
    // The page's IoT tab: the key works on 19044.2905 or later. Other editions have no such condition.
    for (build, spbuild, has_key) in [
        (19044, Some(2904), false),
        (19044, Some(2905), true),
        (19044, Some(1), false),
        (19044, None, false),
        (19043, Some(2905), false),
        (19045, Some(2905), false),
        (26100, Some(1742), true),
        (26100, None, true),
    ] {
        assert_eq!(
            generic_at("IoTEnterpriseS", build, spbuild).is_some(),
            has_key,
            "IoTEnterpriseS {build}.{spbuild:?}"
        );
    }
    for edition_id in ["EnterpriseS", "EnterpriseSN"] {
        assert!(generic_at(edition_id, 19044, Some(1)).is_some(), "{edition_id}");
        assert!(generic_at(edition_id, 19041, None).is_some(), "{edition_id}");
    }
}

#[skuld::test]
fn an_edition_id_is_matched_in_any_case() {
    assert_eq!(generic("iotenterprises", 26100), generic("IoTEnterpriseS", 26100));
    assert!(generic("iotenterprises", 26100).is_some());
}
