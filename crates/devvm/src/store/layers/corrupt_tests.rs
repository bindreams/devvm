use std::path::{Path, PathBuf};

use crate::ErrorKind;
use crate::store::layers::LayerRef;
use crate::test_support::fixtures::unicode_home;

use super::layers_tests::{Env, build, env, key, names};

fn meta_path(env: &Env, layer: &LayerRef) -> PathBuf {
    env.store.generation_dir(layer).join("meta.json")
}

fn quarantined(env: &Env) -> Vec<PathBuf> {
    let root = env.dirs.quarantine();
    names(&root).into_iter().map(|name| root.join(name)).collect()
}

#[skuld::test]
fn corrupt_meta_is_a_miss_then_quarantined_and_rebuilt(#[fixture(unicode_home)] home: &Path) {
    let env = env(home);
    let first = build(&env, key(1), None);
    let old = first.tip().layer_ref();
    drop(first);
    std::fs::write(meta_path(&env, &old), b"{ torn").unwrap();

    // A plain lookup only reports a miss; it never moves anything.
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
    assert!(env.store.generation_dir(&old).is_dir());
    assert!(
        env.progress.messages().iter().any(|m| m.contains("unreadable")),
        "{:?}",
        env.progress.messages()
    );

    // A build repairs under the key's build lock, then builds anew.
    let rebuilt = build(&env, key(1), None);
    assert_ne!(rebuilt.tip().meta().generation, old.generation);
    assert!(
        !env.store.generation_dir(&old).exists(),
        "the corrupt generation was moved away"
    );
    assert_eq!(quarantined(&env).len(), 1);
    assert!(env.store.lookup(&key(1)).unwrap().is_some());
}

#[skuld::test]
fn quarantined_data_is_kept(#[fixture(unicode_home)] home: &Path) {
    let env = env(home);
    let first = build(&env, key(1), None);
    let old = first.tip().layer_ref();
    drop(first);
    std::fs::write(meta_path(&env, &old), b"garbage bytes").unwrap();
    drop(build(&env, key(1), None));

    let [moved] = quarantined(&env).try_into().unwrap();
    assert_eq!(std::fs::read(moved.join("meta.json")).unwrap(), b"garbage bytes");
    assert_eq!(std::fs::read(moved.join("disk.qcow2")).unwrap(), b"disk");
    assert!(
        moved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&key(1).to_string())
    );
}

#[skuld::test]
fn corrupt_meta_of_a_leased_generation_is_not_moved(#[fixture(unicode_home)] home: &Path) {
    let env = env(home);
    let live = build(&env, key(1), None);
    let old = live.tip().layer_ref();
    std::fs::write(meta_path(&env, &old), b"{ torn").unwrap();

    assert!(env.store.lookup(&key(1)).unwrap().is_none());
    let rebuilt = build(&env, key(1), None);
    assert_ne!(rebuilt.tip().meta().generation, old.generation);

    // The holder of the lease still has its directory, with every file readable.
    assert_eq!(
        std::fs::read(live.tip().dir().path().join("disk.qcow2")).unwrap(),
        b"disk"
    );
    assert!(env.store.generation_dir(&old).is_dir());
    // Only a copy of the record went to quarantine.
    let [copy] = quarantined(&env).try_into().unwrap();
    assert_eq!(std::fs::read(copy.join("meta.json")).unwrap(), b"{ torn");
    assert!(!copy.join("disk.qcow2").exists());
}

#[skuld::test]
fn torn_current_file_is_quarantined(#[fixture(unicode_home)] home: &Path) {
    let torn: [&[u8]; 4] = [b"", b"not hex at all", b"abcd", b"0123456789abcdef0123456789abcdef00"];
    for (i, bytes) in torn.into_iter().enumerate() {
        let sub = home.join(format!("case {i}"));
        std::fs::create_dir(&sub).unwrap();
        let env = env(&sub);
        let first = build(&env, key(1), None);
        let old = first.tip().layer_ref();
        drop(first);
        let current = env.dirs.layers().join(key(1).to_string()).join("current");
        std::fs::write(&current, bytes).unwrap();

        assert!(env.store.lookup(&key(1)).unwrap().is_none(), "case {i}");
        let rebuilt = build(&env, key(1), None);
        assert_ne!(rebuilt.tip().meta().generation, old.generation, "case {i}");
        let [moved] = quarantined(&env).try_into().unwrap();
        assert_eq!(std::fs::read(moved.join("current")).unwrap(), bytes, "case {i}");
        assert!(env.store.lookup(&key(1)).unwrap().is_some(), "case {i}");
    }
}

#[skuld::test]
fn newer_schema_is_an_error_and_left_in_place(#[fixture(unicode_home)] home: &Path) {
    let env = env(home);
    let first = build(&env, key(1), None);
    let old = first.tip().layer_ref();
    drop(first);
    let future = format!(
        "{{\"schema\": 2, \"key\": \"{}\", \"generation\": \"{}\", \"layout\": {{\"shards\": [1, 2]}}, \"created\": 12345}}",
        old.key, old.generation
    );
    std::fs::write(meta_path(&env, &old), &future).unwrap();

    for error in [
        env.store.lookup(&key(1)).map(|_| ()).unwrap_err(),
        env.store
            .build(&super::layers_tests::request(key(1)), None, |_| {
                Ok(super::Built { expires: None })
            })
            .map(|_| ())
            .unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::Store);
        assert!(error.message().contains("written by a newer devvm"), "{error}");
        assert!(error.message().contains("meta.json"), "{error}");
    }
    assert_eq!(std::fs::read_to_string(meta_path(&env, &old)).unwrap(), future);
    assert!(quarantined(&env).is_empty());
}

#[skuld::test]
fn a_record_naming_another_layer_is_a_miss(#[fixture(unicode_home)] home: &Path) {
    let env = env(home);
    let first = build(&env, key(1), None);
    let old = first.tip().layer_ref();
    drop(first);
    let text = std::fs::read_to_string(meta_path(&env, &old)).unwrap();
    std::fs::write(
        meta_path(&env, &old),
        text.replace(&key(1).to_string(), &key(9).to_string()),
    )
    .unwrap();
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
}
