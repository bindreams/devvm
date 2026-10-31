use super::Typed;

fn slash(c: char) -> bool {
    c == '/'
}

fn typed(root: &str, typed: &str) -> Typed {
    Typed::new(root.to_owned(), typed.to_owned(), slash)
}

#[skuld::test]
fn the_root_is_named_exactly_as_typed() {
    assert_eq!(typed("/home/u/x", "~/x").name("/home/u/x".to_owned()), "~/x");
    assert_eq!(typed("/b", "a/../b/").name("/b".to_owned()), "a/../b/");
    assert_eq!(typed("/a/b", "/a//b").name("/a/b".to_owned()), "/a//b");
}

#[skuld::test]
fn a_path_below_the_root_is_the_typed_root_and_the_rest() {
    assert_eq!(typed("/home/u/x", "~/x").name("/home/u/x/a/b".to_owned()), "~/x/a/b");
    assert_eq!(typed("/b", "a/../b/").name("/b/c".to_owned()), "a/../b/c");
    assert_eq!(typed("/b/", "a/../b//").name("/b/c".to_owned()), "a/../b/c");
}

#[skuld::test]
fn a_typed_root_that_is_only_separators_keeps_them() {
    assert_eq!(typed("/", "/").name("/a".to_owned()), "/a");
    assert_eq!(typed("/", "//").name("/a".to_owned()), "//a");
}

#[skuld::test]
fn the_separator_of_the_path_is_kept() {
    let windows = Typed::new(r"C:\x".to_owned(), r"C:\y\..\x\".to_owned(), |c| c == '/' || c == '\\');
    assert_eq!(windows.name(r"C:\x\a".to_owned()), r"C:\y\..\x\a");
}

#[skuld::test]
fn a_path_that_is_not_below_the_root_is_unchanged() {
    assert_eq!(typed("/ab", "~/ab").name("/abc".to_owned()), "/abc");
    assert_eq!(
        typed("/a/b", "~/b").name("/a/.devvm-partial-1".to_owned()),
        "/a/.devvm-partial-1"
    );
}

#[skuld::test]
fn no_typed_text_names_every_path_as_it_is() {
    assert_eq!(Typed::none().name("/any/where".to_owned()), "/any/where");
}
