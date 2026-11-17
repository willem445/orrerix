//! Where `build.rs` copies the sideloaded ConPTY host (#3897).
//!
//! `build.rs` places `conpty.dll` + `OpenConsole.exe` in the profile dir it
//! derives from `OUT_DIR`, and portable-pty only uses them when they sit next to
//! the running executable. When cargo moved to its build-dir layout v2 the old
//! fixed ancestor count landed one directory short, the copy went to
//! `<target>/<profile>/build`, and every dev build and test fell back to the inbox
//! conhost with nothing red to say so until `job_object.rs` hung on it. These pin
//! the derivation on both layouts' real `OUT_DIR` shapes.

use loomux_lib::buildpaths::profile_dir_from_out_dir;
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
