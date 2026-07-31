use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use nexa_core::FingerprintBuilder;
use nexa_diagnostics::SourceIdentity;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! string_identity {
    ($name:ident, $validator:ident, $kind:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                if !$validator(&value) {
                    return Err(IdentityError::Invalid { kind: $kind, value });
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn valid_package_id(value: &str) -> bool {
    let mut segments = value.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    valid_package_segment(first)
        && segments.clone().next().is_some()
        && segments.all(valid_package_segment)
}

fn valid_package_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn valid_source_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn valid_dependency_alias(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

string_identity!(PackageId, valid_package_id, "package id");
string_identity!(SourceId, valid_source_id, "source id");
string_identity!(DependencyAlias, valid_dependency_alias, "dependency alias");

/// A canonical dotted module name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModulePath(String);

impl ModulePath {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() || !value.split('.').all(valid_module_segment) {
            return Err(IdentityError::Invalid {
                kind: "module path",
                value,
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn basename(&self) -> &str {
        self.0.rsplit('.').next().unwrap_or(self.0.as_str())
    }

    #[must_use]
    pub fn source_path(&self) -> NormalizedPackagePath {
        let mut value = String::from("src/");
        value.push_str(&self.0.replace('.', "/"));
        value.push_str(".nexa");
        // A validated module path always maps to a valid normalized package path.
        NormalizedPackagePath(value)
    }

    pub fn from_source_path(path: &NormalizedPackagePath) -> Result<Self, IdentityError> {
        let path = path.as_str();
        let relative = path
            .strip_prefix("src/")
            .and_then(|value| value.strip_suffix(".nexa"))
            .ok_or_else(|| IdentityError::Invalid {
                kind: "module source path",
                value: path.to_owned(),
            })?;
        Self::new(relative.replace('/', "."))
    }
}

fn valid_module_segment(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl fmt::Display for ModulePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ModulePath {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ModulePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModulePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A UTF-8, slash-separated path relative to a package-source root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NormalizedPackagePath(pub(crate) String);

impl NormalizedPackagePath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, IdentityError> {
        let value = value.as_ref();
        if value.is_empty()
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\\')
            || value.split('/').any(|part| {
                part.is_empty() || matches!(part, "." | "..") || part.as_bytes().contains(&0)
            })
        {
            return Err(IdentityError::InvalidPath(value.to_owned()));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn from_path(value: &Path) -> Result<Self, IdentityError> {
        let value = value
            .to_str()
            .ok_or_else(|| IdentityError::NonUtf8Path(value.to_path_buf()))?;
        Self::new(value)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    #[must_use]
    pub fn join(&self, child: &NormalizedPackagePath) -> Self {
        Self(format!("{}/{}", self.0, child.0))
    }

    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| Self(parent.to_owned()))
    }
}

impl fmt::Display for NormalizedPackagePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for NormalizedPackagePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NormalizedPackagePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A dependency path as written in a package manifest.
///
/// Parent components are accepted because sibling libraries are common, but resolution must occur
/// against a normalized package directory and cannot escape the package-source root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DependencyPath(String);

impl DependencyPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, IdentityError> {
        let value = value.as_ref();
        if value.is_empty()
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\\')
            || value.split('/').any(|part| part.is_empty() || part == ".")
            || value.as_bytes().contains(&0)
        {
            return Err(IdentityError::InvalidDependencyPath(value.to_owned()));
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn resolve_from(
        &self,
        package_directory: &NormalizedPackagePath,
    ) -> Result<NormalizedPackagePath, IdentityError> {
        let mut components = package_directory
            .as_str()
            .split('/')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for component in self.0.split('/') {
            if component == ".." {
                if components.pop().is_none() {
                    return Err(IdentityError::DependencyEscapesRoot(self.0.clone()));
                }
            } else {
                components.push(component.to_owned());
            }
        }
        NormalizedPackagePath::new(components.join("/"))
    }
}

impl fmt::Display for DependencyPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for DependencyPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DependencyPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Stable incremental identity for one source input.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    pub package_id: PackageId,
    pub path: NormalizedPackagePath,
}

impl SourceKey {
    #[must_use]
    pub const fn new(package_id: PackageId, path: NormalizedPackagePath) -> Self {
        Self { package_id, path }
    }
}

/// Produces the stable package-key representation used to associate an external source range
/// with its exact [`SourceIdentity`] snapshot.
///
/// Package-relative identities retain their readable path. Absolute paths and URIs receive a
/// deterministic synthetic path while the artifact source registry continues to store the
/// original identity verbatim.
#[must_use]
pub fn external_source_key(identity: &SourceIdentity) -> SourceKey {
    let package_id = identity
        .package_id()
        .and_then(|package| PackageId::new(package).ok())
        .unwrap_or_else(|| PackageId::new("nexa.external").expect("static package ID is valid"));
    let path = NormalizedPackagePath::new(identity.path()).unwrap_or_else(|_| {
        let mut builder = FingerprintBuilder::new("nexa.external-source-key", 1);
        match identity.package_id() {
            Some(package_id) => {
                builder.field_u8("has-package", 1);
                builder.field_str("package-id", package_id);
            }
            None => builder.field_u8("has-package", 0),
        }
        builder.field_str("path", identity.path());
        let mut fingerprint = String::with_capacity(64);
        for byte in builder.finish_bytes() {
            write!(&mut fingerprint, "{byte:02x}").expect("writing into String cannot fail");
        }
        NormalizedPackagePath::new(format!("__external/{fingerprint}.source"))
            .expect("fingerprinted external source path is normalized")
    });
    SourceKey::new(package_id, path)
}

/// Dense source identity allocated only inside one final artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ArtifactFileId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityError {
    Invalid { kind: &'static str, value: String },
    InvalidPath(String),
    NonUtf8Path(PathBuf),
    InvalidDependencyPath(String),
    DependencyEscapesRoot(String),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { kind, value } => write!(formatter, "invalid {kind}: {value:?}"),
            Self::InvalidPath(path) => write!(formatter, "invalid package-relative path: {path:?}"),
            Self::NonUtf8Path(path) => {
                write!(formatter, "path is not valid UTF-8: {}", path.display())
            }
            Self::InvalidDependencyPath(path) => {
                write!(formatter, "invalid dependency path: {path:?}")
            }
            Self::DependencyEscapesRoot(path) => {
                write!(formatter, "dependency path escapes source root: {path:?}")
            }
        }
    }
}

impl std::error::Error for IdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths_are_lowercase_and_map_exactly_to_sources() {
        let module = ModulePath::new("food.effects").unwrap();
        assert_eq!(module.source_path().as_str(), "src/food/effects.nexa");
        assert_eq!(
            ModulePath::from_source_path(&module.source_path()).unwrap(),
            module
        );
        assert!(ModulePath::new("Food.effects").is_err());
        assert!(ModulePath::new("food.2effects").is_err());
    }

    #[test]
    fn dependency_resolution_cannot_escape_source_root() {
        let package = NormalizedPackagePath::new("packages/app").unwrap();
        assert_eq!(
            DependencyPath::new("../common")
                .unwrap()
                .resolve_from(&package)
                .unwrap()
                .as_str(),
            "packages/common"
        );
        assert!(
            DependencyPath::new("../../../../escape")
                .unwrap()
                .resolve_from(&package)
                .is_err()
        );
    }
}
