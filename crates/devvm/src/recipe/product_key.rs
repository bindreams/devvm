//! The Windows product key of a recipe's ISO: the key a recipe gives, and the generic key devvm knows for an edition
//! when it gives none.

use secrecy::{ExposeSecret, SecretString};

#[cfg(test)]
#[path = "product_key_tests.rs"]
mod product_key_tests;

/// A Windows product key: five groups of five ASCII letters and digits, such as `XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`.
///
/// The key is a secret: it has no `Display`, and its `Debug` output hides it. [`ProductKey::expose`] is the one way to
/// read it, for the answer file and the layer key.
#[derive(Clone, Debug)]
pub struct ProductKey(SecretString);

impl ProductKey {
    const GROUPS: usize = 5;
    const GROUP_LEN: usize = 5;

    /// The key `text` spells, in uppercase (Windows keys are case-insensitive, so one key has one spelling), or `None`
    /// when it is not five groups of five letters and digits joined by `-`.
    pub fn parse(text: &str) -> Option<ProductKey> {
        let groups: Vec<&str> = text.split('-').collect();
        let shaped = groups.len() == Self::GROUPS
            && groups
                .iter()
                .all(|group| group.len() == Self::GROUP_LEN && group.bytes().all(|b| b.is_ascii_alphanumeric()));
        shaped.then(|| ProductKey(SecretString::from(text.to_ascii_uppercase())))
    }

    /// The generic key Microsoft publishes for the Windows edition `edition_id` (`EDITIONID` of the image, in any case)
    /// at `build` (`BUILD` of its version), or `None` when [`GENERIC_KEYS`] has none: an edition or a build it does
    /// not list, or an image that names no build.
    ///
    /// This is the one place that maps an image to a key.
    pub(crate) fn generic_for(edition_id: &str, build: Option<u32>) -> Option<ProductKey> {
        let build = build?;
        GENERIC_KEYS
            .iter()
            .find(|row| row.matches(edition_id, build))
            .and_then(|row| ProductKey::parse(row.key))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl PartialEq for ProductKey {
    fn eq(&self, other: &ProductKey) -> bool {
        self.expose() == other.expose()
    }
}

impl Eq for ProductKey {}

/// The builds of Windows a [`Generic`] key is for: inclusive ranges, `(first, last)`.
type Builds = &'static [(u32, u32)];

/// A generic volume license key (GVLK) of an edition: the key a KMS client installs with. Setup takes it for a volume
/// edition, which is installed unactivated.
struct Generic {
    edition_id: &'static str,
    builds: Builds,
    key: &'static str,
}

impl Generic {
    fn matches(&self, edition_id: &str, build: u32) -> bool {
        self.edition_id.eq_ignore_ascii_case(edition_id)
            && self.builds.iter().any(|&(first, last)| (first..=last).contains(&build))
    }
}

const fn row(edition_id: &'static str, builds: Builds, key: &'static str) -> Generic {
    Generic {
        edition_id,
        builds,
        key,
    }
}

/// Windows 10 (build 10240) and every release since: the page lists one key per client edition, not one per release.
const WINDOWS_10_AND_11: Builds = &[(10240, u32::MAX)];
/// Windows Enterprise LTSC 2019 (17763), 2021 and 2024 (26100), which share their keys. LTSC 2021 is Windows 10 21H2: the
/// system reports build 19044, but an `install.wim` is the 2004 image that 21H2 turns on by an enablement package, so
/// it most likely records 19041. The whole family 19041 to 19045 is one release.
const LTSC_2019_ON: Builds = &[(17763, 17763), (19041, 19045), (26100, 26100)];
/// Windows IoT Enterprise LTSC 2021 (the 19041 to 19045 family, as above) and 2024.
const IOT_LTSC_2021_ON: Builds = &[(19041, 19045), (26100, 26100)];
const LTSB_2016: Builds = &[(14393, 14393)];
const LTSB_2015: Builds = &[(10240, 10240)];
const SERVER_2016: Builds = &[(14393, 14393)];
const SERVER_2019: Builds = &[(17763, 17763)];
const SERVER_2022: Builds = &[(20348, 20348)];
const SERVER_2025: Builds = &[(26100, 26100)];

/// The generic keys of the editions devvm can install, copied on 2026-11-18 from Microsoft's page
/// <https://learn.microsoft.com/en-us/windows-server/get-started/kms-client-activation-keys> (last updated by Microsoft
/// on 2025-06-25). Keys of a released version do not change, and nothing checks this table against the page.
///
/// What is not here has no key and is refused, so a recipe names one with `product-key`: the Windows versions the guest
/// cannot be built from (before Windows 10 and Server 2016), the retired Semi-Annual Channel servers, an edition the
/// page does not list (Home, Enterprise multi-session, the IoT Enterprise subscription LTSC), and the two it lists whose edition id no image
/// devvm has read shows (Server Essentials and Datacenter: Azure Edition). A new LTSC or Server release is added when
/// the page lists it, and an edition with an image that shows its edition id.
const GENERIC_KEYS: &[Generic] = &[
    // Windows 11 and Windows 10 Semi-Annual Channel
    row("Professional", WINDOWS_10_AND_11, "W269N-WFGWX-YVC9B-4J6C9-T83GX"),
    row("ProfessionalN", WINDOWS_10_AND_11, "MH37W-N47XK-V7XM9-C7227-GCQG9"),
    row(
        "ProfessionalWorkstation",
        WINDOWS_10_AND_11,
        "NRG8B-VKK3Q-CXVCJ-9G2XF-6Q84J",
    ),
    row(
        "ProfessionalWorkstationN",
        WINDOWS_10_AND_11,
        "9FNHH-K3HBT-3W4TD-6383H-6XYWF",
    ),
    row(
        "ProfessionalEducation",
        WINDOWS_10_AND_11,
        "6TP4R-GNPTD-KYYHQ-7B7DP-J447Y",
    ),
    row(
        "ProfessionalEducationN",
        WINDOWS_10_AND_11,
        "YVWGF-BXNMC-HTQYQ-CPQ99-66QFC",
    ),
    row("Education", WINDOWS_10_AND_11, "NW6C2-QMPVW-D7KKK-3GKT6-VCFB2"),
    row("EducationN", WINDOWS_10_AND_11, "2WH4N-8QGBV-H22JP-CT43Q-MDWWJ"),
    row("Enterprise", WINDOWS_10_AND_11, "NPPR9-FWDCX-D2C8J-H872K-2YT43"),
    row("EnterpriseN", WINDOWS_10_AND_11, "DPH2V-TTNVB-4X9Q3-TJR4H-KHJW4"),
    row("EnterpriseG", WINDOWS_10_AND_11, "YYVX9-NTFWV-6MDM3-9PT4T-4M68B"),
    row("EnterpriseGN", WINDOWS_10_AND_11, "44RPN-FTY23-9VTTB-MP9BX-T84FV"),
    // Windows Enterprise LTSC and LTSB
    row("EnterpriseS", LTSC_2019_ON, "M7XTQ-FN8P6-TTKYV-9D4CC-J462D"),
    row("EnterpriseSN", LTSC_2019_ON, "92NFX-8DJQP-P6BBQ-THF9C-7CG2H"),
    row("IoTEnterpriseS", IOT_LTSC_2021_ON, "KBN8V-HFGQ4-MGXVD-347P6-PDQGT"),
    row("EnterpriseS", LTSB_2016, "DCPHK-NFMTC-H88MJ-PFHPY-QJ4BJ"),
    row("EnterpriseSN", LTSB_2016, "QFFDN-GRT3P-VKWWX-X7T3R-8B639"),
    row("EnterpriseS", LTSB_2015, "WNMTR-4C88C-JK8YV-HQ7T2-76DF9"),
    row("EnterpriseSN", LTSB_2015, "2F77B-TNFGY-69QQF-B8YKP-D69TJ"),
    // Windows Server LTSC
    row("ServerStandard", SERVER_2025, "TVRH6-WHNXV-R9WG3-9XRFY-MY832"),
    row("ServerDatacenter", SERVER_2025, "D764K-2NDRG-47T6Q-P8T8W-YP6DF"),
    row("ServerStandard", SERVER_2022, "VDYBN-27WPP-V4HQT-9VMD4-VMK7H"),
    row("ServerDatacenter", SERVER_2022, "WX4NM-KYWYW-QJJR4-XV3QB-6VM33"),
    row("ServerStandard", SERVER_2019, "N69G4-B89J2-4G8F4-WWYCC-J464C"),
    row("ServerDatacenter", SERVER_2019, "WMDGN-G9PQG-XVVXX-R3X43-63DFG"),
    row("ServerStandard", SERVER_2016, "WC2BQ-8NRM3-FDDYY-2BFGV-KHKQY"),
    row("ServerDatacenter", SERVER_2016, "CB7KF-BWN84-R7R2Y-793K2-8XDDG"),
];
