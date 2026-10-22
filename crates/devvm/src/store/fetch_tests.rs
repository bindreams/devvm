use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256 as Sha2};

use crate::store::fetch::{Artifact, ContentRange, Fetcher, PartOpener, PartWriter, Response, Resume, Transport};
use crate::test_support::fixtures::{MemoryProgress, anna_home};
use crate::{Dirs, ErrorKind, Result, Sha256};

const URL: &str = "https://example.test/files/disk.img";
const ETAG: &str = "\"v1\"";

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn sha_of(data: &[u8]) -> Sha256 {
    Sha256::from_bytes(Sha2::digest(data).into())
}

fn artifact(data: &[u8]) -> Artifact {
    Artifact {
        url: URL.to_owned(),
        sha256: sha_of(data),
    }
}

/// A body that yields `data`, then fails with a connection error if `fail` is set.
struct Body {
    data: Cursor<Vec<u8>>,
    fail: bool,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.data.read(buf)? {
            0 if self.fail => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset",
            )),
            n => Ok(n),
        }
    }
}

fn response(status: u16, data: Vec<u8>, length: Option<u64>, fail: bool) -> Response {
    Response {
        status,
        content_range: None,
        content_length: length,
        validator: Some(ETAG.to_owned()),
        body: Box::new(Body {
            data: Cursor::new(data),
            fail,
        }),
    }
}

/// What a well-behaved server answers: the whole file, or the tail from `resume.from`.
fn serve(data: &[u8], resume: Option<&Resume>) -> Response {
    let total = data.len() as u64;
    match resume {
        None => response(200, data.to_vec(), Some(total), false),
        Some(resume) if resume.from >= total => Response {
            content_range: Some(ContentRange::Unsatisfied { total }),
            ..response(416, Vec::new(), Some(0), false)
        },
        Some(resume) => Response {
            content_range: Some(ContentRange::Range {
                start: resume.from,
                end: total - 1,
                total: Some(total),
            }),
            ..response(
                206,
                data[resume.from as usize..].to_vec(),
                Some(total - resume.from),
                false,
            )
        },
    }
}

type Handler = Box<dyn Fn(usize, Option<&Resume>) -> Result<Response> + Send + Sync>;

/// A transport whose n-th request is answered by `handler(n, resume)`; it records every request.
struct Script {
    handler: Handler,
    requests: Mutex<Vec<Option<Resume>>>,
}

impl Script {
    fn new(handler: impl Fn(usize, Option<&Resume>) -> Result<Response> + Send + Sync + 'static) -> Arc<Script> {
        Arc::new(Script {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn serving(data: Vec<u8>) -> Arc<Script> {
        Script::new(move |_, resume| Ok(serve(&data, resume)))
    }

    fn requests(&self) -> Vec<Option<Resume>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Transport for Script {
    fn get(&self, _url: &str, resume: Option<Resume>) -> Result<Response> {
        let n = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(resume.clone());
            requests.len() - 1
        };
        (self.handler)(n, resume.as_ref())
    }
}

struct Setup {
    fetcher: Fetcher,
    downloads: PathBuf,
    progress: Arc<MemoryProgress>,
}

fn setup(home: &Path, transport: Arc<Script>) -> Setup {
    let downloads = Dirs::under(home).downloads();
    let progress = Arc::new(MemoryProgress::default());
    let fetcher = Fetcher::new(downloads.clone(), transport, progress.clone());
    Setup {
        fetcher,
        downloads,
        progress,
    }
}

fn listing(dir: &Path) -> Vec<String> {
    crate::store::layers::layers_tests::names(dir)
}

fn part(setup: &Setup, artifact: &Artifact) -> PathBuf {
    setup.downloads.join(format!("{}.part", artifact.sha256))
}

fn sidecar(setup: &Setup, artifact: &Artifact) -> PathBuf {
    setup.downloads.join(format!("{}.part.json", artifact.sha256))
}

// Happy path ==========================================================================================================

#[skuld::test]
fn fetch_verifies_publishes_and_reuses(#[fixture(anna_home)] home: &Path) {
    let data = bytes(300_000, 1);
    let transport = Script::serving(data.clone());
    let setup = setup(home, transport.clone());
    let artifact = artifact(&data);

    let path = setup.fetcher.fetch(&artifact).unwrap();
    assert_eq!(path, setup.fetcher.entry_path(&artifact));
    assert_eq!(path.file_name().unwrap(), "disk.img");
    assert_eq!(std::fs::read(&path).unwrap(), data);
    assert_eq!(
        listing(&setup.downloads),
        vec![artifact.sha256.to_string(), format!("{}.lock", artifact.sha256)]
    );

    let again = setup.fetcher.fetch(&artifact).unwrap();
    assert_eq!(again, path);
    assert_eq!(transport.requests().len(), 1, "a verified entry needs no request");
}

// Failures that end in a deleted partial ==============================================================================

#[skuld::test]
fn wrong_sha256_deletes_the_partial_and_reports_url_expected_and_actual(#[fixture(anna_home)] home: &Path) {
    let served = bytes(10_000, 1);
    let expected = bytes(10_000, 2);
    let setup = setup(home, Script::serving(served.clone()));
    let wanted = artifact(&expected);

    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Checksum);
    assert!(error.message().contains(URL), "{error}");
    assert!(error.message().contains(&wanted.sha256.to_string()), "{error}");
    assert!(error.message().contains(&sha_of(&served).to_string()), "{error}");
    assert!(!part(&setup, &wanted).exists() && !sidecar(&setup, &wanted).exists());
    assert!(!setup.fetcher.entry_path(&wanted).exists());
}

#[skuld::test]
fn truncated_artifact_is_a_checksum_error_and_leaves_no_entry(#[fixture(anna_home)] home: &Path) {
    let full = bytes(50_000, 3);
    let short = full[..20_000].to_vec();
    // The server reports a complete, shorter body: the hash is what notices.
    let transport = Script::new(move |_, _| Ok(response(200, short.clone(), Some(20_000), false)));
    let setup = setup(home, transport);
    let wanted = artifact(&full);

    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Checksum);
    assert!(!part(&setup, &wanted).exists() && !sidecar(&setup, &wanted).exists());
    assert!(!setup.fetcher.entry_path(&wanted).exists());
}

#[skuld::test]
fn remote_change_between_attempts_is_a_checksum_error_with_no_partial(#[fixture(anna_home)] home: &Path) {
    let original = bytes(100_000, 4);
    let changed = bytes(100_000, 5);
    let (first, second) = (original.clone(), changed.clone());
    let transport = Script::new(move |n, resume| match n {
        0 => Ok(response(200, first[..40_000].to_vec(), Some(100_000), true)),
        _ => {
            assert!(resume.is_some(), "the second attempt resumes");
            // If-Range failed on the server: a new ETag and the whole new file.
            Ok(Response {
                validator: Some("\"v2\"".into()),
                ..response(200, second.clone(), Some(100_000), false)
            })
        }
    });
    let setup = setup(home, transport.clone());
    let wanted = artifact(&original);

    assert_eq!(setup.fetcher.fetch(&wanted).unwrap_err().kind(), ErrorKind::Download);
    assert_eq!(std::fs::metadata(part(&setup, &wanted)).unwrap().len(), 40_000);

    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Checksum);
    assert!(!part(&setup, &wanted).exists() && !sidecar(&setup, &wanted).exists());
    assert_eq!(
        transport.requests()[1],
        Some(Resume {
            from: 40_000,
            validator: Some(ETAG.into())
        })
    );
}

#[skuld::test]
fn server_ignoring_if_range_and_sending_206_of_new_content_is_caught_by_the_hash(#[fixture(anna_home)] home: &Path) {
    let original = bytes(100_000, 4);
    let changed = bytes(100_000, 5);
    let (first, second) = (original.clone(), changed.clone());
    let transport = Script::new(move |n, resume| match n {
        0 => Ok(response(200, first[..40_000].to_vec(), Some(100_000), true)),
        _ => Ok(serve(&second, resume)),
    });
    let setup = setup(home, transport);
    let wanted = artifact(&original);

    assert_eq!(setup.fetcher.fetch(&wanted).unwrap_err().kind(), ErrorKind::Download);
    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Checksum);
    assert!(!part(&setup, &wanted).exists() && !sidecar(&setup, &wanted).exists());
}

// Interruptions keep the partial ======================================================================================

#[skuld::test]
fn body_cut_off_midway_keeps_the_part_and_the_next_fetch_resumes(#[fixture(anna_home)] home: &Path) {
    let data = bytes(2_000_000, 6);
    let served = Arc::new(AtomicUsize::new(0));
    let (handler_data, handler_served) = (data.clone(), served.clone());
    let transport = Script::new(move |n, resume| {
        let response = match n {
            0 => response(200, handler_data[..700_000].to_vec(), Some(2_000_000), true),
            _ => serve(&handler_data, resume),
        };
        let counted = match (n, resume) {
            (0, _) => 700_000,
            (_, Some(r)) => handler_data.len() - r.from as usize,
            _ => handler_data.len(),
        };
        handler_served.fetch_add(counted, Ordering::SeqCst);
        Ok(response)
    });
    let setup = setup(home, transport.clone());
    let wanted = artifact(&data);

    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Download);
    assert!(
        error.message().contains(URL) && error.message().contains("700000"),
        "{error}"
    );
    assert_eq!(std::fs::metadata(part(&setup, &wanted)).unwrap().len(), 700_000);
    assert!(sidecar(&setup, &wanted).is_file());

    let path = setup.fetcher.fetch(&wanted).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(
        transport.requests()[1],
        Some(Resume {
            from: 700_000,
            validator: Some(ETAG.into())
        })
    );
    assert_eq!(served.load(Ordering::SeqCst), data.len(), "every byte was served once");
    assert!(
        setup
            .progress
            .messages()
            .iter()
            .any(|m| m.contains("resuming") && m.contains("700000"))
    );
    assert!(!part(&setup, &wanted).exists() && !sidecar(&setup, &wanted).exists());
}

#[skuld::test]
fn connection_dropped_mid_body_is_an_interruption_not_a_checksum_error(#[fixture(anna_home)] home: &Path) {
    let data = bytes(60_000, 7);
    let short = data[..10_000].to_vec();
    // The body ends cleanly but short of the declared length.
    let transport = Script::new(move |_, _| Ok(response(200, short.clone(), Some(60_000), false)));
    let setup = setup(home, transport);
    let wanted = artifact(&data);

    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Download);
    assert!(error.message().contains("10000 of 60000"), "{error}");
    assert_eq!(std::fs::metadata(part(&setup, &wanted)).unwrap().len(), 10_000);
}

#[skuld::test]
fn unexpected_http_status_is_a_download_error_naming_url_and_status(#[fixture(anna_home)] home: &Path) {
    for status in [404u16, 500] {
        let sub = home.join(status.to_string());
        std::fs::create_dir(&sub).unwrap();
        let transport = Script::new(move |_, _| Ok(response(status, Vec::new(), None, false)));
        let setup = setup(&sub, transport);
        let wanted = artifact(b"x");
        let error = setup.fetcher.fetch(&wanted).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Download);
        assert!(
            error.message().contains(URL) && error.message().contains(&status.to_string()),
            "{error}"
        );
        assert!(!setup.fetcher.entry_path(&wanted).exists());
    }
}

#[skuld::test]
fn unexpected_status_on_resume_keeps_the_part(#[fixture(anna_home)] home: &Path) {
    let data = bytes(100_000, 8);
    let first = data.clone();
    let transport = Script::new(move |n, _| match n {
        0 => Ok(response(200, first[..30_000].to_vec(), Some(100_000), true)),
        _ => Ok(response(503, Vec::new(), None, false)),
    });
    let setup = setup(home, transport);
    let wanted = artifact(&data);
    assert_eq!(setup.fetcher.fetch(&wanted).unwrap_err().kind(), ErrorKind::Download);
    let error = setup.fetcher.fetch(&wanted).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Download);
    assert!(error.message().contains("503"), "{error}");
    assert_eq!(std::fs::metadata(part(&setup, &wanted)).unwrap().len(), 30_000);
}

#[skuld::test]
fn a_cached_entry_clears_a_stale_sidecar(#[fixture(anna_home)] home: &Path) {
    let data = bytes(10_000, 6);
    let wanted = artifact(&data);
    let setup = setup(home, Script::serving(data.clone()));
    setup.fetcher.fetch(&wanted).unwrap();
    // A crash between publishing and deleting the sidecar leaves these behind.
    std::fs::write(sidecar(&setup, &wanted), b"{}").unwrap();
    std::fs::write(setup.downloads.join(format!("{}.part.json.tmp", wanted.sha256)), b"{}").unwrap();
    setup.fetcher.fetch(&wanted).unwrap();
    assert_eq!(
        listing(&setup.downloads),
        vec![wanted.sha256.to_string(), format!("{}.lock", wanted.sha256)]
    );
}

// Resume corner cases =================================================================================================

/// Leaves `have` bytes of `data` as a part from `URL`, as an interrupted run would.
fn seed_part(setup: &Setup, artifact: &Artifact, data: &[u8], have: usize, url: &str) {
    std::fs::create_dir_all(&setup.downloads).unwrap();
    std::fs::write(part(setup, artifact), &data[..have]).unwrap();
    let json = format!("{{\"url\":\"{url}\",\"validator\":\"{}\"}}", ETAG.replace('"', "\\\""));
    std::fs::write(sidecar(setup, artifact), json).unwrap();
}

#[skuld::test]
fn complete_part_with_416_is_verified_and_published(#[fixture(anna_home)] home: &Path) {
    let data = bytes(5_000, 9);
    let transport = Script::serving(data.clone());
    let setup = setup(home, transport.clone());
    let wanted = artifact(&data);
    seed_part(&setup, &wanted, &data, data.len(), URL);

    let path = setup.fetcher.fetch(&wanted).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(
        transport.requests(),
        vec![Some(Resume {
            from: 5_000,
            validator: Some(ETAG.into())
        })]
    );
}

#[skuld::test]
fn a_416_with_a_different_total_restarts(#[fixture(anna_home)] home: &Path) {
    let data = bytes(5_000, 9);
    let handler_data = data.clone();
    let transport = Script::new(move |n, resume| match (n, resume) {
        (0, Some(_)) => Ok(Response {
            content_range: Some(ContentRange::Unsatisfied { total: 4_000 }),
            ..response(416, Vec::new(), Some(0), false)
        }),
        _ => Ok(serve(&handler_data, resume)),
    });
    let setup = setup(home, transport.clone());
    let wanted = artifact(&data);
    seed_part(&setup, &wanted, &bytes(5_000, 77), 2_000, URL);

    let path = setup.fetcher.fetch(&wanted).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(transport.requests().len(), 2);
    assert_eq!(transport.requests()[1], None, "the restart asks for the whole file");
}

#[skuld::test]
fn content_range_start_mismatch_restarts(#[fixture(anna_home)] home: &Path) {
    let data = bytes(5_000, 9);
    let handler_data = data.clone();
    let transport = Script::new(move |n, resume| match (n, resume) {
        (0, Some(_)) => Ok(Response {
            content_range: Some(ContentRange::Range {
                start: 123,
                end: 4_999,
                total: Some(5_000),
            }),
            ..response(206, handler_data[123..].to_vec(), Some(4_877), false)
        }),
        _ => Ok(serve(&handler_data, resume)),
    });
    let setup = setup(home, transport.clone());
    let wanted = artifact(&data);
    seed_part(&setup, &wanted, &data, 2_000, URL);

    let path = setup.fetcher.fetch(&wanted).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(transport.requests()[1], None);
}

#[skuld::test]
fn a_part_from_another_url_is_not_resumed(#[fixture(anna_home)] home: &Path) {
    let data = bytes(5_000, 9);
    let transport = Script::serving(data.clone());
    let setup = setup(home, transport.clone());
    let wanted = artifact(&data);
    seed_part(&setup, &wanted, &data, 2_000, "https://mirror.test/other");

    setup.fetcher.fetch(&wanted).unwrap();
    assert_eq!(transport.requests(), vec![None]);
}

// Disk full ===========================================================================================================

/// Writes to the real file until `budget` bytes are written, then fails with `kind`.
struct FullOpener {
    kind: std::io::ErrorKind,
    budget: usize,
}

struct FullWriter {
    file: std::fs::File,
    kind: std::io::ErrorKind,
    left: usize,
}

impl PartWriter for FullWriter {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if bytes.len() > self.left {
            self.file.write_all(&bytes[..self.left])?;
            self.left = 0;
            return Err(std::io::Error::new(self.kind, "no space left on device"));
        }
        self.left -= bytes.len();
        self.file.write_all(bytes)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_all()
    }
}

impl PartOpener for FullOpener {
    fn open(&self, path: &Path, append: bool) -> std::io::Result<Box<dyn PartWriter>> {
        let mut options = std::fs::OpenOptions::new();
        if append {
            options.append(true).create(true);
        } else {
            options.write(true).create(true).truncate(true);
        }
        Ok(Box::new(FullWriter {
            file: options.open(path)?,
            kind: self.kind,
            left: self.budget,
        }))
    }
}

#[skuld::test]
fn disk_full_is_a_clear_error_and_publishes_nothing(#[fixture(anna_home)] home: &Path) {
    for kind in [std::io::ErrorKind::StorageFull, std::io::ErrorKind::QuotaExceeded] {
        let sub = home.join(format!("{kind:?}"));
        std::fs::create_dir(&sub).unwrap();
        let data = bytes(500_000, 10);
        let downloads = Dirs::under(&sub).downloads();
        let fetcher = Fetcher::with_part_opener(
            downloads.clone(),
            Script::serving(data.clone()),
            Arc::new(MemoryProgress::default()),
            Arc::new(FullOpener { kind, budget: 200_000 }),
        );
        let wanted = artifact(&data);

        let error = fetcher.fetch(&wanted).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DiskFull, "{kind:?}: {error}");
        let part = downloads.join(format!("{}.part", wanted.sha256));
        assert!(error.message().contains(&part.display().to_string()), "{error}");
        assert_eq!(
            std::fs::metadata(&part).unwrap().len(),
            200_000,
            "the part is kept for a resume"
        );
        assert!(!fetcher.entry_path(&wanted).exists());
    }
}

// File names ==========================================================================================================

#[skuld::test]
fn hostile_url_segments_stay_inside_the_downloads_dir(#[fixture(anna_home)] home: &Path) {
    let hundred_dots = format!("{}.", "a".repeat(99));
    let urls = [
        "https://h/..".to_owned(),
        "https://h/..%2f..%2fx".to_owned(),
        "https://h/con".to_owned(),
        "https://h/CON.txt".to_owned(),
        "https://h/CON.tar.gz".to_owned(),
        "https://h/nul.".to_owned(),
        "https://h/COM1".to_owned(),
        "https://h/lpt9.img".to_owned(),
        "https://h/CONIN%24".to_owned(),
        "https://h/CONOUT$".to_owned(),
        "https://h/a.".to_owned(),
        format!("https://h/{hundred_dots}"),
        format!("https://h/{}.", "b".repeat(300)),
        "https://h/x.tar.gz?token=abc#frag".to_owned(),
        "https://h/".to_owned(),
        "https://h".to_owned(),
        "https://h/%00".to_owned(),
        format!("https://h/{}", "c".repeat(300)),
        "https://h/a%5Cb".to_owned(),
        "https://h/a\\b".to_owned(),
        "https://h/%2e%2e".to_owned(),
        "not a url at all".to_owned(),
    ];
    let downloads = Dirs::under(home).downloads();
    for url in urls {
        let wanted = Artifact {
            url: url.clone(),
            sha256: sha_of(b"x"),
        };
        let fetcher = Fetcher::new(
            downloads.clone(),
            Script::serving(Vec::new()),
            Arc::new(MemoryProgress::default()),
        );
        let entry = fetcher.entry_path(&wanted);
        let dir = downloads.join(wanted.sha256.to_string());
        assert_eq!(entry.parent().unwrap(), dir, "{url}");
        let name = entry.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.is_empty() && name.len() <= 100, "{url}: {name}");
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
            "{url}: {name}"
        );
        assert!(!name.starts_with('.') && !name.ends_with('.'), "{url}: {name}");
        let stem = name.split('.').next().unwrap().to_ascii_uppercase();
        assert!(
            !["CON", "PRN", "AUX", "NUL", "COM1", "COM9", "LPT1", "LPT9"].contains(&stem.as_str()),
            "{url}: {name}"
        );
    }
    let name = |url: &str| {
        let wanted = Artifact {
            url: url.to_owned(),
            sha256: sha_of(b"x"),
        };
        Fetcher::new(
            downloads.clone(),
            Script::serving(Vec::new()),
            Arc::new(MemoryProgress::default()),
        )
        .entry_path(&wanted)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
    };
    assert_eq!(name("https://h/x.tar.gz?token=abc#frag"), "x.tar.gz");
    assert_eq!(name("https://h/ubuntu%2024.img"), "ubuntu_24.img");
    assert_eq!(name("https://h/CON.tar.gz"), "artifact");
    assert_eq!(name("https://h/disk.img"), "disk.img");
}

// Progress ============================================================================================================

#[skuld::test]
fn progress_lines_name_the_url_and_bytes(#[fixture(anna_home)] home: &Path) {
    let data = bytes(2_621_440, 11);
    let setup = setup(home, Script::serving(data.clone()));
    setup.fetcher.fetch(&artifact(&data)).unwrap();
    assert_eq!(
        setup.progress.messages(),
        vec![
            format!("downloading {URL}: 0 of 2621440 bytes"),
            format!("downloading {URL}: 1048576 of 2621440 bytes"),
            format!("downloading {URL}: 2097152 of 2621440 bytes"),
        ]
    );
}

#[skuld::test]
fn fetcher_is_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<Fetcher>();
}

// Restart ordering ====================================================================================================

struct DenyOpener;

impl PartOpener for DenyOpener {
    fn open(&self, _path: &Path, _append: bool) -> std::io::Result<Box<dyn PartWriter>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "part file locked by a scanner",
        ))
    }
}

#[skuld::test]
fn failed_restart_keeps_the_old_validator_with_the_old_part(#[fixture(anna_home)] home: &Path) {
    let stale = bytes(100_000, 4);
    let good = bytes(100_000, 5);
    let (stale_head, current) = (stale.clone(), good.clone());
    let v2 = "\"v2\"";
    let transport = Script::new(move |n, resume| match n {
        0 => Ok(response(200, stale_head[..40_000].to_vec(), Some(100_000), true)),
        // The resume's If-Range no longer matches, so the server sends the whole new file.
        1 => Ok(Response {
            validator: Some(v2.to_owned()),
            ..response(200, current.clone(), Some(100_000), false)
        }),
        // A server honouring If-Range: a v2 validator continues, anything else gets the whole file.
        _ => match resume.and_then(|resume| resume.validator.as_deref()) {
            Some(validator) if validator == v2 => Ok(Response {
                validator: Some(v2.to_owned()),
                ..serve(&current, resume)
            }),
            _ => Ok(Response {
                validator: Some(v2.to_owned()),
                ..serve(&current, None)
            }),
        },
    });
    let wanted = artifact(&good);
    let downloads = Dirs::under(home).downloads();
    let progress = Arc::new(MemoryProgress::default());

    let first = Fetcher::new(downloads.clone(), transport.clone(), progress.clone());
    assert_eq!(first.fetch(&wanted).unwrap_err().kind(), ErrorKind::Download);
    let denied = Fetcher::with_part_opener(
        downloads.clone(),
        transport.clone(),
        progress.clone(),
        Arc::new(DenyOpener),
    );
    assert_eq!(denied.fetch(&wanted).unwrap_err().kind(), ErrorKind::Io);

    let healthy = Fetcher::new(downloads, transport, progress);
    let entry = healthy
        .fetch(&wanted)
        .unwrap_or_else(|e| panic!("the remote is healthy, yet: {e}"));
    assert_eq!(std::fs::read(entry).unwrap(), good);
}
