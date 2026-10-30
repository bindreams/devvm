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
    // (source, slash, destination, result)
    let table: &[(Source, bool, Present, Result<Place, Conflict>)] = &[
        // A directory becomes the destination, whatever is there, and never nests.
        (Source::Dir, false, Nothing, Ok(Root)),
        (Source::Dir, true, Nothing, Ok(Root)),
        (Source::Dir, false, Dir, Ok(Root)),
        (Source::Dir, true, Dir, Ok(Root)),
        (Source::Dir, false, File, Err(Conflict::DirOntoNonDir)),
        (Source::Dir, true, File, Err(Conflict::DirOntoNonDir)),
        // A destination that is a link counts as what it leads to: a directory is followed, with or without a slash,
        // and anything else is a file.
        (Source::Dir, false, Link(ToDir), Ok(Root)),
        (Source::Dir, true, Link(ToDir), Ok(Root)),
        (Source::Dir, false, Link(NotDir), Err(Conflict::DirOntoNonDir)),
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
            kind: Kind::Dir { mtime: None },
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

#[skuld::test]
fn a_clash_is_looked_for_only_below_what_the_copy_does_not_create() {
    use Beyond::Dir as ToDir;
    use Present::{Dir, File, Link, Nothing};
    let rules = Rules {
        from: "host",
        to: "the guest",
        problem: &|_| None,
        key: &|name| name.to_owned(),
    };
    let file = |path: &str| Item {
        rel: path.split('/').map(str::to_owned).collect(),
        kind: Kind::File {
            executable: false,
            mtime: None,
            size: 0,
        },
    };
    let dir = |path: &str| items(&[path]).pop().unwrap();
    // `d` is a directory, `d/f` a file; what the destination holds at each path, and which paths are looked up.
    let tree = [dir("d"), file("d/f")];
    // (root starts empty, what is at `d`, what is at `d/f`, paths looked up, error expected)
    let table: &[(bool, Present, Present, &[&str], bool)] = &[
        (true, Dir, Dir, &[], false),
        (false, Nothing, Dir, &["d"], false),
        (false, Link(ToDir), Dir, &["d"], false),
        (false, Dir, Nothing, &["d", "d/f"], false),
        (false, Dir, Link(ToDir), &["d", "d/f"], false),
        (false, Dir, Dir, &["d", "d/f"], true),
        (false, File, Nothing, &["d"], true),
    ];
    for (root_fresh, at_d, at_f, looked_up, fails) in table {
        let mut asked = Vec::new();
        let result = super::check_clashes(
            &tree,
            *root_fresh,
            &rules,
            &mut |rel| {
                asked.push(rel.join("/"));
                Ok(if rel.len() == 1 { *at_d } else { *at_f })
            },
            &|rel| (rel.join("/"), format!("/dest/{}", rel.join("/"))),
        );
        assert_eq!(asked, *looked_up, "{root_fresh} {at_d:?} {at_f:?}");
        assert_eq!(result.is_err(), *fails, "{root_fresh} {at_d:?} {at_f:?}: {result:?}");
    }
}

#[skuld::test]
fn the_root_of_a_merged_tree_is_fresh_only_when_missing() {
    use Beyond::{Dir as ToDir, NotDir};
    use Present::{Dir, File, Link, Nothing};
    for (present, fresh) in [
        (Nothing, true),
        (Dir, false),
        (Link(ToDir), false),
        (Link(NotDir), false),
        (File, false),
    ] {
        assert_eq!(super::root_is_fresh(present), fresh, "{present:?}");
    }
}
