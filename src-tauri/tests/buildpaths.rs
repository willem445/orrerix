//! Where `build.rs` copies the sideloaded ConPTY host (#3897).
//!
//! `build.rs` places `conpty.dll` + `OpenConsole.exe` in the profile dir it
//! derives from `OUT_DIR`, and portable-pty only uses them when they sit next to
//! the running executable. When cargo moved to its build-dir layout v2 the old
//! fixed ancestor count landed one directory short, the copy went to
//! `<target>/<profile>/build`, and every dev build and test fell back to the inbox
//! conhost with nothing red to say so until `job_object.rs` hung on it. These pin
//! the derivation on both layouts' real `OUT_DIR` shapes.

use loomux_lib::buildpaths::{conhost_destination, profile_dir_from_out_dir};
use std::path::Path;

#[test]
fn the_original_layout_resolves_to_the_profile_dir() {
    assert_eq!(
        profile_dir_from_out_dir(Path::new("target/debug/build/orrerix-0123abcd/out")),
        Some(Path::new("target/debug"))
    );
}

/// The shape cargo produced on CI under Rust 1.100.0 (#3897 run 44115430973:
/// `target\debug\build\orrerix\07fc8cbbbeed86c5\out`) — one level deeper than the
/// original, which is the whole of the regression.
#[test]
fn the_build_dir_v2_layout_resolves_to_the_same_profile_dir() {
    assert_eq!(
        profile_dir_from_out_dir(Path::new("target/debug/build/orrerix/0123abcd/out")),
        Some(Path::new("target/debug"))
    );
    assert_eq!(
        profile_dir_from_out_dir(Path::new("target/release/build/orrerix/0123abcd/out")),
        Some(Path::new("target/release"))
    );
}

/// A checkout path can carry a `build` component of its own; the layout's is the
/// NEAREST one, and taking any other would copy outside the target dir entirely.
#[test]
fn a_build_component_above_the_target_dir_is_not_the_layouts() {
    assert_eq!(
        profile_dir_from_out_dir(Path::new(
            "ci/build/repo/target/release/build/orrerix/0123abcd/out"
        )),
        Some(Path::new("ci/build/repo/target/release"))
    );
}

/// No `build` ancestor means no recognisable layout: copy nothing rather than
/// guess a directory.
#[test]
fn an_out_dir_with_no_build_ancestor_yields_nothing() {
    assert_eq!(profile_dir_from_out_dir(Path::new("somewhere/else/out")), None);
}

/// An OUT_DIR no layout explains must not cost the modern host silently: with the
/// fixture answering the host's cursor query, `job_object.rs` passes on the inbox
/// conhost too, so this warning is the only thing that says the copy missed
/// (#3897 review follow-up). It names the OUT_DIR it could not read and what the
/// build lost, which is what a developer reading a build log needs to act on it.
#[test]
fn an_unrecognised_layout_warns_instead_of_copying_nothing_silently() {
    let out_dir = Path::new("somewhere/else/out");
    let warning = conhost_destination(out_dir).expect_err("no build ancestor must warn");
    assert!(!warning.trim().is_empty(), "the warning is silent: {warning:?}");
    assert!(
        warning.contains(&out_dir.display().to_string()),
        "the warning does not name the OUT_DIR it could not place: {warning:?}"
    );
    assert!(
        warning.contains("conpty.dll") && warning.contains("inbox"),
        "the warning does not say what the build lost: {warning:?}"
    );
}

/// cargo reads `cargo:warning=` to the end of the line; a line break would turn
/// the rest of the message into a malformed build-script directive.
#[test]
fn the_warning_is_one_line() {
    let warning = conhost_destination(Path::new("somewhere/else/out")).unwrap_err();
    assert!(
        !warning.contains('\n') && !warning.contains('\r'),
        "the cargo:warning text spans lines: {warning:?}"
    );
}

/// The other side: a layout the derivation knows copies, and warns about nothing —
/// a warning on every build would be noise nobody reads.
#[test]
fn a_recognised_layout_copies_without_a_warning() {
    for (out_dir, profile_dir) in [
        ("target/debug/build/orrerix-0123abcd/out", "target/debug"),
        ("target/debug/build/orrerix/0123abcd/out", "target/debug"),
        ("ci/build/repo/target/release/build/orrerix/0123abcd/out", "ci/build/repo/target/release"),
    ] {
        assert_eq!(
            conhost_destination(Path::new(out_dir)),
            Ok(Path::new(profile_dir)),
            "{out_dir}"
        );
    }
}
