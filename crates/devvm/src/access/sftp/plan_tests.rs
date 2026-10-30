use super::{
    Beyond, Conflict, Item, Kind, Place, Present, Rules, Source, base_name, check_listing, check_names, parent_of,
    place,
};
use crate::ErrorKind;
use crate::host::names;

#[skuld::test]
fn where_a_source_lands_depends_only_on_the_source_the_slash_and_the_destination() {
    use Beyond::{Dir as ToDir, NotDir};
    use Place::{At, Inside, Root};
    use Present::{Dir, File, Link, Nothing};
    let follow = |follow| Root { follow };
    // (source, slash, destination, result)
    let table: &[(Source, bool, Present, Result<Place, Conflict>)] = &[
        // A directory becomes the destination, whatever is there, and never nests.
        (Source::Dir, false, Nothing, Ok(follow(false))),
        (Source::Dir, true, Nothing, Ok(follow(false))),
        (Source::Dir, false, Dir, Ok(follow(false))),
        (Source::Dir, true, Dir, Ok(follow(false))),
        (Source::Dir, false, File, Err(Conflict::DirOntoNonDir)),
        (Source::Dir, true, File, Err(Conflict::DirOntoNonDir)),
        // A link at the destination is replaced, unless the user wrote a slash and it leads to a directory.
        (Source::Dir, false, Link(ToDir), Ok(follow(false))),
        (Source::Dir, true, Link(ToDir), Ok(follow(true))),
        (Source::Dir, false, Link(NotDir), Ok(follow(false))),
        (Source::Dir, true, Link(NotDir), Err(Conflict::DirOntoNonDir)),
        // An entry goes inside a directory, and is the destination otherwise.
        (Source::Entry, false, Dir, Ok(Inside)),
        (Source::Entry, true, Dir, Ok(Inside)),
        (Source::Entry, false, Link(ToDir), Ok(Inside)),
        (Source::Entry, true, Link(ToDir), Ok(Inside)),
        (Source::Entry, false, Nothing, Ok(At)),
        (Source::Entry, true, Nothing, Ok(Inside)),
        (Source::Entry, false, File, Ok(At)),
        (Source::Entry, true, File, Err(Conflict::SlashOnNonDir)),
        (Source::Entry, false, Link(NotDir), Ok(At)),
        (Source::Entry, true, Link(NotDir), Err(Conflict::SlashOnNonDir)),
    ];
    for (source, slash, dest, want) in table {
        assert_eq!(place(*source, *slash, *dest), *want, "{source:?} {slash} {dest:?}");
    }
}

fn items(paths: &[&str]) -> Vec<Item> {
    paths
        .iter()
        .map(|path| Item {
            rel: path.split('/').map(str::to_owned).collect(),
            kind: Kind::Dir,
        })
        .collect()
}

#[skuld::test]
fn a_target_refuses_the_names_it_cannot_store_and_two_names_it_takes_for_one() {
    let rules = Rules {
        from: "host",
        to: "the guest",
        problem: &names::windows_problem,
        key: &names::windows_key,
    };
    check_names(&items(&["a", "a/b", "c.txt"]), &rules).unwrap();
    for (paths, expect) in [
        (&["ok", "ok/a?b"][..], "\"a?b\""),
        (&["con"], "\"con\""),
        (&["x."], "\"x.\""),
        (&["Readme", "README"], "\"README\""),
        (&["d/Readme", "d/README"], "\"d/README\""),
    ] {
        let error = check_names(&items(paths), &rules).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{paths:?}: {error}");
        assert!(error.message().contains(expect), "{paths:?}: {error}");
        assert!(error.message().contains("the guest"), "{paths:?}: {error}");
    }
    // The same names pass where the target compares exactly.
    let posix = Rules {
        from: "guest",
        to: "this host",
        problem: &names::posix_problem,
        key: &|name| name.to_owned(),
    };
    check_names(&items(&["Readme", "README", "con", "a?b"]), &posix).unwrap();
}

/// No real file system lists a name twice, so the fake server cannot be made to; the check is tested directly.
#[skuld::test]
fn a_listing_that_names_one_entry_twice_is_refused() {
    let names = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();
    check_listing("/t", &names(&["a", "b", "c"])).unwrap();
    let error = check_listing("/t", &names(&["a", "b", "b", "c"])).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("\"b\" twice"), "{error}");
}

#[skuld::test]
fn guest_path_pieces() {
    assert_eq!(parent_of("/a/b"), "/a");
    assert_eq!(parent_of("/a/b/"), "/a");
    assert_eq!(parent_of("/a"), "/");
    assert_eq!(parent_of("/"), "/");
    assert_eq!(parent_of("a"), ".");
    assert_eq!(base_name("/a/b/"), Some("b"));
    assert_eq!(base_name("/"), None);
}
