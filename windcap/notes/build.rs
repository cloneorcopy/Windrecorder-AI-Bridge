// The Windows version resource, so Explorer's Properties tab and `windnotes --version` say the
// same thing about this .exe. The field list and the wording live in one place for all eleven
// binaries; this file only names which of the eleven it is. See that file for why it is included
// rather than linked, and for the offline-resolution check that let `embed-resource` be used.
include!("../base/version_resource.rs");

fn main() {
    version_resource("windnotes");
}
