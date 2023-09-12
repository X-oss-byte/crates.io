use cargo_toml::{Dependency, DepsSet, Error, Inheritable, Manifest, Package};

pub fn validate_manifest(manifest: &Manifest) -> Result<(), Error> {
    let package = manifest.package.as_ref();

    // Check that a `[package]` table exists in the manifest, since crates.io
    // does not accept workspace manifests.
    let package = package.ok_or(Error::Other("missing field `package`"))?;

    validate_package(package)?;

    // These checks ensure that dependency workspace inheritance has been
    // normalized by cargo before publishing.
    if manifest.dependencies.is_inherited()
        || manifest.dev_dependencies.is_inherited()
        || manifest.build_dependencies.is_inherited()
    {
        return Err(Error::InheritedUnknownValue);
    }

    Ok(())
}

pub fn validate_package(package: &Package) -> Result<(), Error> {
    // These checks ensure that package field workspace inheritance has been
    // normalized by cargo before publishing.
    if package.edition.is_inherited()
        || package.rust_version.is_inherited()
        || package.version.is_inherited()
        || package.authors.is_inherited()
        || package.description.is_inherited()
        || package.homepage.is_inherited()
        || package.documentation.is_inherited()
        || package.readme.is_inherited()
        || package.keywords.is_inherited()
        || package.categories.is_inherited()
        || package.exclude.is_inherited()
        || package.include.is_inherited()
        || package.license.is_inherited()
        || package.license_file.is_inherited()
        || package.repository.is_inherited()
        || package.publish.is_inherited()
    {
        return Err(Error::InheritedUnknownValue);
    }

    // Check that the `rust-version` field has a valid value, if it exists.
    if let Some(rust_version) = package.rust_version() {
        validate_rust_version(rust_version)?;
    }

    Ok(())
}

trait IsInherited {
    fn is_inherited(&self) -> bool;
}

impl<T> IsInherited for Inheritable<T> {
    fn is_inherited(&self) -> bool {
        !self.is_set()
    }
}

impl<T: IsInherited> IsInherited for Option<T> {
    fn is_inherited(&self) -> bool {
        self.as_ref().map(|it| it.is_inherited()).unwrap_or(false)
    }
}

impl IsInherited for Dependency {
    fn is_inherited(&self) -> bool {
        matches!(self, Dependency::Inherited(_))
    }
}

impl IsInherited for DepsSet {
    fn is_inherited(&self) -> bool {
        self.iter().any(|(_key, dep)| dep.is_inherited())
    }
}

pub fn validate_rust_version(value: &str) -> Result<(), Error> {
    match semver::VersionReq::parse(value) {
        // Exclude semver operators like `^` and pre-release identifiers
        Ok(_) if value.chars().all(|c| c.is_ascii_digit() || c == '.') => Ok(()),
        Ok(_) | Err(..) => Err(Error::Other("invalid `rust-version` value")),
    }
}
