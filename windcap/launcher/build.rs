// The Windows resources, so Explorer's Properties tab and `Windrecorder --version` say the same thing
// about this .exe, and so the file a stranger double-clicks carries the product's icon rather than the
// generic one. The field list and the wording live in one place for every binary; this file only names
// which binary it is. See that fragment for why it is `include!`d rather than linked.
//
// `icons\1-app.ico` is the declaration of the icon, by the directory convention the fragment
// implements: every `icons\<id>-<name>.ico` becomes an icon resource under ordinal `<id>`, and the
// first group is the one Explorer shows for the file. This crate loads nothing at run time -- it has
// no window of its own -- so the resource exists purely for the shell.
include!("../base/version_resource.rs");

fn main() {
    version_resource("Windrecorder");
}
