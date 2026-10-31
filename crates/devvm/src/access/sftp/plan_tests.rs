use super::{
    Beyond, Conflict, Item, Kind, Place, Present, Rules, Source, base_name, check_destination, check_listing,
    check_names, normalize_user, parent_of, place, time_notes,
};
use std::collections::HashSet;

use crate::guest::Guests;
use crate::guest::fake::FakeGuests;
use crate::host::names;
use crate::{ErrorKind, GuestKind};

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
        times: "",
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
        times: "",
    };
    check_names(&items(&["Readme", "README", "con", "a?b"]), &posix).unwrap();
}

/// A look-up that fails for a name below one the target cannot store (Windows: `out\a:b\y` is not a path) must not hide
/// the refusal of the name above it; a look-up that fails below a name that exists is a real failure.
#[skuld::test]
fn a_failed_look_up_below_a_name_the_target_cannot_store_still_names_that_name() {
    let rules = Rules {
        from: "guest",
        to: "this host",
        problem: &names::windows_problem,
        key: &names::windows_key,
        times: "",
    };
    let chain = |names: &[&str]| {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| (Some((*name).to_owned()), names[..=i].join("/")))
            .collect::<Vec<_>>()
    };
    let invalid = || crate::Error::new(ErrorKind::Io, "cannot inspect: invalid name");
    // Only `y` fails to be looked up, as the target cannot read a path through `a:b`; nothing exists.
    let error = check_destination(&chain(&["out", "a:b", "y"]), &rules, &mut |path: &String| {
        if path.ends_with("/y") {
            Err(invalid())
        } else {
            Ok(Present::Nothing)
        }
    })
    .unwrap_err();
    assert!(error.message().contains("\"a:b\""), "{error}");
    assert!(error.message().contains("cannot be created"), "{error}");
    // `out` exists and nothing in the chain is a bad name: the failure to look `y` up is the error.
    let error = check_destination(
        &chain(&["out", "ok", "y"]),
        &rules,
        &mut |path: &String| match path.as_str() {
            "out" => Ok(Present::Dir),
            "out/ok/y" => Err(invalid()),
            _ => Ok(Present::Nothing),
        },
    )
    .unwrap_err();
    assert!(error.message().contains("invalid name"), "{error}");
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
        times: "",
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
    // (root starts empty, what is at `d`, what is at `d/f`, paths looked up, directories the copy creates or None for
    // an error). The root is the empty path.
    #[allow(clippy::type_complexity)]
    let table: &[(bool, Present, Present, &[&str], Option<&[&str]>)] = &[
        (true, Dir, Dir, &[], Some(&["", "d"])),
        (false, Nothing, Dir, &["d"], Some(&["d"])),
        (false, Link(ToDir), Dir, &["d"], Some(&["d"])),
        (false, Dir, Nothing, &["d", "d/f"], Some(&[])),
        (false, Dir, Link(ToDir), &["d", "d/f"], Some(&[])),
        (false, Dir, Dir, &["d", "d/f"], None),
        (false, File, Nothing, &["d"], None),
    ];
    for (root_fresh, at_d, at_f, looked_up, creates) in table {
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
        match creates {
            None => assert!(result.is_err(), "{root_fresh} {at_d:?} {at_f:?}: {result:?}"),
            Some(creates) => {
                let want: HashSet<String> = creates.iter().map(|path| (*path).to_owned()).collect();
                let got: HashSet<String> = result.unwrap().iter().map(|rel| rel.join("/")).collect();
                assert_eq!(got, want, "{root_fresh} {at_d:?} {at_f:?}");
            }
        }
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

#[skuld::test]
fn a_guest_destination_is_resolved_by_name() {
    let linux = FakeGuests.os(GuestKind::Linux).unwrap();
    for (given, want) in [
        ("/h/new/.", "/h/new"),
        ("/h/a/../b", "/h/b"),
        ("/h/a/b/../../c", "/h/c"),
        ("/h/./x//y", "/h/x/y"),
        ("/h/a/..", "/h"),
        ("/..", "/"),
        ("/a/../..", "/"),
        ("/", "/"),
        ("new/.", "new"),
        ("a/../b", "b"),
        ("a/..", "."),
        (".", "."),
        ("..", ".."),
        ("../a", "../a"),
        ("a/../../b", "../b"),
        ("/.hidden/..x", "/.hidden/..x"),
    ] {
        assert_eq!(normalize_user(given, &*linux).unwrap(), want, "{given}");
    }
}

/// A root stays a root and a `..` above it stays at it, whatever the guest calls a root.
#[skuld::test]
fn a_guest_root_stays_a_root_and_dotdot_does_not_go_above_it() {
    let linux = FakeGuests.os(GuestKind::Linux).unwrap();
    let windows = FakeGuests.os(GuestKind::Windows).unwrap();
    let on_drive = FakeGuests.os_on_drive(GuestKind::Linux, "Z:");
    type Cases<'a> = &'a [(&'a str, &'a str)];
    let table: &[(&dyn crate::guest::GuestOs, Cases)] = &[
        (
            &*on_drive,
            &[
                ("Z:/", "Z:/"),
                ("Z:", "Z:/"),
                ("Z://", "Z:/"),
                ("Z:/..", "Z:/"),
                ("Z:/a/../..", "Z:/"),
                ("Z:/a/../b", "Z:/b"),
                ("Z:/a/.", "Z:/a"),
                ("Z:/./a//b", "Z:/a/b"),
                ("/a/..", "/"),
            ],
        ),
        (
            &*windows,
            &[
                ("C:/", "C:/"),
                ("c:/../x", "c:/x"),
                ("C:", "C:/"),
                ("C:/a/b/../../..", "C:/"),
                ("//server/share/x", "//server/share/x"),
                ("//server/share", "//server/share/"),
                ("//server/share/", "//server/share/"),
                ("//server/share/..", "//server/share/"),
                ("//server/share/x/../../y", "//server/share/y"),
                ("//server//share/x", "//server/share/x"),
                ("/a/..", "/"),
                ("a/../b", "b"),
            ],
        ),
        // On a guest with no drives, `Z:` is a name and a leading `//` is one `/`.
        (
            &*linux,
            &[
                ("Z:/..", "."),
                ("Z:/a/..", "Z:"),
                ("//server/share/x", "/server/share/x"),
                ("//", "/"),
                ("/..", "/"),
            ],
        ),
    ];
    for (guest, cases) in table {
        for (given, want) in *cases {
            assert_eq!(normalize_user(given, *guest).unwrap(), *want, "{given}");
        }
    }
}

/// `C:a` is relative to the current directory of drive `C:`, which no path of ours names: it is refused, naming it,
/// instead of becoming a path on another drive or none. On a guest with no drives it is a plain name.
#[skuld::test]
fn a_drive_relative_windows_path_is_refused_naming_it() {
    let linux = FakeGuests.os(GuestKind::Linux).unwrap();
    let windows = FakeGuests.os(GuestKind::Windows).unwrap();
    for given in ["C:a/..", "C:a", "c:..", "D:a/b/../c"] {
        let error = normalize_user(given, &*windows).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{given}: {error}");
        assert!(error.message().contains(given), "{given}: {error}");
        assert!(normalize_user(given, &*linux).is_ok(), "{given}");
    }
    for given in ["C:", "C:/", "C:/a"] {
        assert!(normalize_user(given, &*windows).is_ok(), "{given}");
    }
}

#[skuld::test]
fn a_time_that_is_not_kept_is_named_only_for_what_the_copy_sets() {
    let rules = Rules {
        from: "host",
        to: "the guest",
        problem: &|_| None,
        key: &|name| name.to_owned(),
        times: "1970 to 2106",
    };
    let file = |path: &str, mtime| Item {
        rel: path.split('/').map(str::to_owned).collect(),
        kind: Kind::File {
            executable: false,
            mtime,
            size: 0,
        },
    };
    let dir = |path: &str, mtime| Item {
        rel: path.split('/').map(str::to_owned).collect(),
        kind: Kind::Dir { mtime },
    };
    let top = Kind::Dir { mtime: None };
    let tree = [
        dir("made", None),
        dir("kept", None),
        dir("set", Some(5)),
        file("made/lost.txt", None),
        file("made/fine.txt", Some(5)),
    ];
    let path = |rel: &[String]| format!("/src/{}", rel.join("/"));
    let notes = |created: &[&[&str]]| {
        let created: HashSet<Vec<String>> = created
            .iter()
            .map(|rel| rel.iter().map(|part| (*part).to_owned()).collect())
            .collect();
        time_notes(&rules, &top, &tree, &created, &path).join(
            "
",
        )
    };
    // A directory the copy does not create keeps its own time, so nothing is said about it; a file is always written.
    let said = notes(&[&["made"]]);
    assert!(said.contains("of /src/made is not kept"), "{said}");
    assert!(said.contains("of /src/made/lost.txt is not kept"), "{said}");
    assert!(!said.contains("/src/kept"), "{said}");
    assert!(!said.contains("/src/set"), "{said}");
    assert!(!said.contains("/src/made/fine.txt"), "{said}");
    assert!(!said.contains("of /src/ is not kept"), "the root is not made: {said}");
    assert!(said.contains("outside 1970 to 2106"), "{said}");
    // The root is the empty path.
    let said = notes(&[&[]]);
    assert!(said.contains("of /src/ is not kept"), "{said}");
}
