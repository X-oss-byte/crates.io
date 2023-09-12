#[cfg(test)]
#[macro_use]
extern crate claims;

#[cfg(any(feature = "builder", test))]
pub use crate::builder::TarballBuilder;
use crate::limit_reader::LimitErrorReader;
use crate::manifest::validate_manifest;
pub use crate::vcs_info::CargoVcsInfo;
pub use cargo_toml::Manifest;
use flate2::read::GzDecoder;
use std::io::Read;
use std::path::Path;
use tracing::instrument;

#[cfg(any(feature = "builder", test))]
mod builder;
mod limit_reader;
mod manifest;
mod vcs_info;

#[derive(Debug)]
pub struct TarballInfo {
    pub manifest: Manifest,
    pub vcs_info: Option<CargoVcsInfo>,
}

#[derive(Debug, thiserror::Error)]
pub enum TarballError {
    #[error("uploaded tarball is malformed or too large when decompressed")]
    Malformed(#[source] std::io::Error),
    #[error("invalid path found: {0}")]
    InvalidPath(String),
    #[error("unexpected symlink or hard link found: {0}")]
    UnexpectedSymlink(String),
    #[error("Cargo.toml manifest is missing")]
    MissingManifest,
    #[error("Cargo.toml manifest is invalid: {0}")]
    InvalidManifest(#[from] cargo_toml::Error),
    #[error(transparent)]
    IO(#[from] std::io::Error),
}

#[instrument(skip_all, fields(%pkg_name))]
pub fn process_tarball<R: Read>(
    pkg_name: &str,
    tarball: R,
    max_unpack: u64,
) -> Result<TarballInfo, TarballError> {
    // All our data is currently encoded with gzip
    let decoder = GzDecoder::new(tarball);

    // Don't let gzip decompression go into the weeeds, apply a fixed cap after
    // which point we say the decompressed source is "too large".
    let decoder = LimitErrorReader::new(decoder, max_unpack);

    // Use this I/O object now to take a peek inside
    let mut archive = tar::Archive::new(decoder);

    let vcs_info_path = Path::new(&pkg_name).join(".cargo_vcs_info.json");
    let mut vcs_info = None;

    let manifest_path = Path::new(&pkg_name).join("Cargo.toml");
    let manifest_path_lower = Path::new(&pkg_name).join("cargo.toml");
    let mut manifest = None;

    for entry in archive.entries()? {
        let mut entry = entry.map_err(TarballError::Malformed)?;

        // Verify that all entries actually start with `$name-$vers/`.
        // Historically Cargo didn't verify this on extraction so you could
        // upload a tarball that contains both `foo-0.1.0/` source code as well
        // as `bar-0.1.0/` source code, and this could overwrite other crates in
        // the registry!
        let entry_path = entry.path()?;
        if !entry_path.starts_with(pkg_name) {
            return Err(TarballError::InvalidPath(entry_path.display().to_string()));
        }

        // Historical versions of the `tar` crate which Cargo uses internally
        // don't properly prevent hard links and symlinks from overwriting
        // arbitrary files on the filesystem. As a bit of a hammer we reject any
        // tarball with these sorts of links. Cargo doesn't currently ever
        // generate a tarball with these file types so this should work for now.
        let entry_type = entry.header().entry_type();
        if entry_type.is_hard_link() || entry_type.is_symlink() {
            return Err(TarballError::UnexpectedSymlink(
                entry_path.display().to_string(),
            ));
        }

        if entry_path == vcs_info_path {
            let mut contents = String::new();
            entry.read_to_string(&mut contents)?;
            vcs_info = CargoVcsInfo::from_contents(&contents).ok();
        } else if entry_path == manifest_path || entry_path == manifest_path_lower {
            // Try to extract and read the Cargo.toml from the tarball, silently
            // erroring if it cannot be read.
            let mut contents = String::new();
            entry.read_to_string(&mut contents)?;
            manifest = Some({
                let manifest = Manifest::from_str(&contents)?;
                validate_manifest(&manifest)?;
                manifest
            });
        }
    }

    let Some(manifest) = manifest else {
        return Err(TarballError::MissingManifest);
    };

    Ok(TarballInfo { manifest, vcs_info })
}

#[cfg(test)]
mod tests {
    use super::process_tarball;
    use crate::TarballBuilder;
    use cargo_toml::OptionalFile;
    use std::path::Path;

    #[test]
    fn process_tarball_test() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(b"[package]\nname = \"foo\"\nversion = \"0.0.1\"\n")
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        assert_none!(tarball_info.vcs_info);

        assert_err!(process_tarball("bar-0.0.1", &*tarball, limit));
    }

    #[test]
    fn process_tarball_test_incomplete_vcs_info() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(b"[package]\nname = \"foo\"\nversion = \"0.0.1\"\n")
            .add_file("foo-0.0.1/.cargo_vcs_info.json", br#"{"unknown": "field"}"#)
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let vcs_info = assert_some!(tarball_info.vcs_info);
        assert_eq!(vcs_info.path_in_vcs, "");
    }

    #[test]
    fn process_tarball_test_vcs_info() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(b"[package]\nname = \"foo\"\nversion = \"0.0.1\"\n")
            .add_file(
                "foo-0.0.1/.cargo_vcs_info.json",
                br#"{"path_in_vcs": "path/in/vcs"}"#,
            )
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let vcs_info = assert_some!(tarball_info.vcs_info);
        assert_eq!(vcs_info.path_in_vcs, "path/in/vcs");
    }

    #[test]
    fn process_tarball_test_manifest() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
[package]
name = "foo"
version = "0.0.1"
rust-version = "1.59"
readme = "README.md"
repository = "https://github.com/foo/bar"
"#,
            )
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let package = assert_some!(tarball_info.manifest.package);
        assert_some_eq!(package.readme().as_path(), Path::new("README.md"));
        assert_some_eq!(package.repository(), "https://github.com/foo/bar");
        assert_some_eq!(package.rust_version(), "1.59");
    }

    #[test]
    fn process_tarball_test_manifest_with_project() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
                [project]
                name = "foo"
                version = "0.0.1"
                rust-version = "1.23"
                "#,
            )
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let package = assert_some!(tarball_info.manifest.package);
        assert_some_eq!(package.rust_version(), "1.23");
    }

    #[test]
    fn process_tarball_test_manifest_with_default_readme() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
                [package]
                name = "foo"
                version = "0.0.1"
                "#,
            )
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let package = assert_some!(tarball_info.manifest.package);
        assert_matches!(package.readme(), OptionalFile::Flag(true));
    }

    #[test]
    fn process_tarball_test_manifest_with_boolean_readme() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
                [package]
                name = "foo"
                version = "0.0.1"
                readme = false
                "#,
            )
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let package = assert_some!(tarball_info.manifest.package);
        assert_matches!(package.readme(), OptionalFile::Flag(false));
    }

    #[test]
    fn process_tarball_test_lowercase_manifest() {
        let tarball = TarballBuilder::new("foo", "0.0.1")
            .add_file(
                "foo-0.0.1/cargo.toml",
                br#"
[package]
name = "foo"
version = "0.0.1"
repository = "https://github.com/foo/bar"
"#,
            )
            .build();

        let limit = 512 * 1024 * 1024;

        let tarball_info = assert_ok!(process_tarball("foo-0.0.1", &*tarball, limit));
        let package = assert_some!(tarball_info.manifest.package);
        assert_some_eq!(package.repository(), "https://github.com/foo/bar");
    }
}
