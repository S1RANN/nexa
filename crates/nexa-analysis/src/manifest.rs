use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use semver::Version;
use serde::Deserialize;

use crate::{
    DependencyAlias, DependencyPath, IdentityError, ModulePath, NormalizedPackagePath, PackageId,
};

pub const PACKAGE_MANIFEST_SCHEMA: u32 = 2;
pub const PACKAGE_SOURCE_ROOT: &str = "src";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PackageKind {
    Application,
    Library,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationPolicy {
    Required,
    DefaultEnabled,
    UserControlled,
    Programmatic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathDependency {
    pub path: DependencyPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationSettings {
    pub entry: ModulePath,
    pub activation: ActivationPolicy,
    pub priority: i32,
    pub handler_fuel: Option<u64>,
    pub cumulative_budget: Option<u64>,
    pub heap_objects: Option<u32>,
    pub host_resources: Option<u32>,
    pub tasks: Option<u32>,
    pub release_records: Option<usize>,
    pub capabilities: BTreeSet<String>,
    pub entitlement: Option<String>,
}

/// A fully validated `package.toml schema = 2`.
///
/// Application-only fields live in [`ApplicationSettings`]. Libraries cannot accidentally carry
/// lifecycle configuration because the library deserializer does not define those fields and both
/// variants use `deny_unknown_fields`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    pub schema: u32,
    pub kind: PackageKind,
    pub id: PackageId,
    pub name: String,
    pub version: Version,
    pub source_root: NormalizedPackagePath,
    pub dependencies: BTreeMap<DependencyAlias, PathDependency>,
    pub application: Option<ApplicationSettings>,
}

impl PackageManifest {
    pub fn parse(source: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            toml::from_str(source).map_err(|error| ManifestError::Toml(error.to_string()))?;
        match raw {
            RawManifest::Application(raw) => Self::from_application(raw),
            RawManifest::Library(raw) => Self::from_library(raw),
        }
    }

    fn from_application(raw: RawApplicationManifest) -> Result<Self, ManifestError> {
        validate_schema(raw.schema)?;
        let common = ValidatedCommon::new(
            raw.id,
            raw.name,
            &raw.version,
            raw.source_root,
            raw.dependencies,
        )?;
        let entry = ModulePath::new(raw.entry).map_err(ManifestError::Identity)?;
        validate_capabilities(&raw.capabilities)?;
        if raw.entitlement.as_deref().is_some_and(str::is_empty) {
            return Err(ManifestError::InvalidEntitlement);
        }
        Ok(Self {
            schema: PACKAGE_MANIFEST_SCHEMA,
            kind: PackageKind::Application,
            id: common.id,
            name: common.name,
            version: common.version,
            source_root: common.source_root,
            dependencies: common.dependencies,
            application: Some(ApplicationSettings {
                entry,
                activation: raw.activation,
                priority: raw.priority,
                handler_fuel: raw.handler_fuel,
                cumulative_budget: raw.cumulative_budget,
                heap_objects: raw.heap_objects,
                host_resources: raw.host_resources,
                tasks: raw.tasks,
                release_records: raw.release_records,
                capabilities: raw.capabilities.into_iter().collect(),
                entitlement: raw.entitlement,
            }),
        })
    }

    fn from_library(raw: RawLibraryManifest) -> Result<Self, ManifestError> {
        validate_schema(raw.schema)?;
        let common = ValidatedCommon::new(
            raw.id,
            raw.name,
            &raw.version,
            raw.source_root,
            raw.dependencies,
        )?;
        Ok(Self {
            schema: PACKAGE_MANIFEST_SCHEMA,
            kind: PackageKind::Library,
            id: common.id,
            name: common.name,
            version: common.version,
            source_root: common.source_root,
            dependencies: common.dependencies,
            application: None,
        })
    }

    #[must_use]
    pub fn entry(&self) -> Option<&ModulePath> {
        self.application.as_ref().map(|settings| &settings.entry)
    }

    #[must_use]
    pub fn expected_entry_source(&self) -> Option<NormalizedPackagePath> {
        self.entry().map(ModulePath::source_path)
    }

    #[must_use]
    pub const fn is_application(&self) -> bool {
        matches!(self.kind, PackageKind::Application)
    }

    #[must_use]
    pub const fn is_library(&self) -> bool {
        matches!(self.kind, PackageKind::Library)
    }

    /// Deterministic schema-2 TOML used as a build-fingerprint input.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        fn quote(value: &str) -> String {
            toml::Value::String(value.to_owned()).to_string()
        }

        let mut output = String::new();
        writeln!(output, "schema = {}", self.schema).expect("String writes cannot fail");
        writeln!(
            output,
            "kind = {}",
            quote(match self.kind {
                PackageKind::Application => "application",
                PackageKind::Library => "library",
            })
        )
        .expect("String writes cannot fail");
        writeln!(output, "id = {}", quote(self.id.as_str())).expect("String writes cannot fail");
        writeln!(output, "name = {}", quote(&self.name)).expect("String writes cannot fail");
        writeln!(output, "version = {}", quote(&self.version.to_string()))
            .expect("String writes cannot fail");
        writeln!(output, "source_root = {}", quote(self.source_root.as_str()))
            .expect("String writes cannot fail");
        if let Some(application) = &self.application {
            writeln!(output, "entry = {}", quote(application.entry.as_str()))
                .expect("String writes cannot fail");
            writeln!(
                output,
                "activation = {}",
                quote(match application.activation {
                    ActivationPolicy::Required => "required",
                    ActivationPolicy::DefaultEnabled => "default-enabled",
                    ActivationPolicy::UserControlled => "user-controlled",
                    ActivationPolicy::Programmatic => "programmatic",
                })
            )
            .expect("String writes cannot fail");
            writeln!(output, "priority = {}", application.priority)
                .expect("String writes cannot fail");
            write_optional(&mut output, "handler_fuel", application.handler_fuel);
            write_optional(
                &mut output,
                "cumulative_budget",
                application.cumulative_budget,
            );
            write_optional(&mut output, "heap_objects", application.heap_objects);
            write_optional(&mut output, "host_resources", application.host_resources);
            write_optional(&mut output, "tasks", application.tasks);
            write_optional(&mut output, "release_records", application.release_records);
            if !application.capabilities.is_empty() {
                output.push_str("capabilities = [");
                for (index, capability) in application.capabilities.iter().enumerate() {
                    if index != 0 {
                        output.push_str(", ");
                    }
                    output.push_str(&quote(capability));
                }
                output.push_str("]\n");
            }
            if let Some(entitlement) = &application.entitlement {
                writeln!(output, "entitlement = {}", quote(entitlement))
                    .expect("String writes cannot fail");
            }
        }
        if !self.dependencies.is_empty() {
            output.push_str("\n[dependencies]\n");
            for (alias, dependency) in &self.dependencies {
                writeln!(
                    output,
                    "{} = {{ path = {} }}",
                    alias.as_str(),
                    quote(dependency.path.as_str())
                )
                .expect("String writes cannot fail");
            }
        }
        output.into_bytes()
    }
}

fn write_optional<T: fmt::Display>(output: &mut String, key: &str, value: Option<T>) {
    if let Some(value) = value {
        writeln!(output, "{key} = {value}").expect("String writes cannot fail");
    }
}

struct ValidatedCommon {
    id: PackageId,
    name: String,
    version: Version,
    source_root: NormalizedPackagePath,
    dependencies: BTreeMap<DependencyAlias, PathDependency>,
}

impl ValidatedCommon {
    fn new(
        id: String,
        name: String,
        version: &str,
        source_root: String,
        dependencies: BTreeMap<String, RawPathDependency>,
    ) -> Result<Self, ManifestError> {
        if name.trim().is_empty() {
            return Err(ManifestError::EmptyName);
        }
        if source_root != PACKAGE_SOURCE_ROOT {
            return Err(ManifestError::InvalidSourceRoot(source_root));
        }
        let source_root =
            NormalizedPackagePath::new(source_root).map_err(ManifestError::Identity)?;
        let version = Version::parse(version)
            .map_err(|error| ManifestError::InvalidVersion(error.to_string()))?;
        let dependencies = dependencies
            .into_iter()
            .map(|(alias, dependency)| {
                Ok((
                    DependencyAlias::new(alias).map_err(ManifestError::Identity)?,
                    PathDependency {
                        path: DependencyPath::new(dependency.path)
                            .map_err(ManifestError::Identity)?,
                    },
                ))
            })
            .collect::<Result<_, ManifestError>>()?;
        Ok(Self {
            id: PackageId::new(id).map_err(ManifestError::Identity)?,
            name,
            version,
            source_root,
            dependencies,
        })
    }
}

fn validate_schema(schema: u32) -> Result<(), ManifestError> {
    if schema == PACKAGE_MANIFEST_SCHEMA {
        Ok(())
    } else {
        Err(ManifestError::UnsupportedSchema(schema))
    }
}

fn validate_capabilities(capabilities: &[String]) -> Result<(), ManifestError> {
    let mut unique = BTreeSet::new();
    for capability in capabilities {
        if capability.is_empty()
            || capability.split('.').any(str::is_empty)
            || !capability
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ManifestError::InvalidCapability(capability.clone()));
        }
        if !unique.insert(capability) {
            return Err(ManifestError::DuplicateCapability(capability.clone()));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum RawManifest {
    Application(RawApplicationManifest),
    Library(RawLibraryManifest),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApplicationManifest {
    schema: u32,
    id: String,
    name: String,
    version: String,
    source_root: String,
    entry: String,
    activation: ActivationPolicy,
    #[serde(default)]
    priority: i32,
    handler_fuel: Option<u64>,
    cumulative_budget: Option<u64>,
    heap_objects: Option<u32>,
    host_resources: Option<u32>,
    tasks: Option<u32>,
    release_records: Option<usize>,
    #[serde(default)]
    capabilities: Vec<String>,
    entitlement: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, RawPathDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLibraryManifest {
    schema: u32,
    id: String,
    name: String,
    version: String,
    source_root: String,
    #[serde(default)]
    dependencies: BTreeMap<String, RawPathDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathDependency {
    path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    Toml(String),
    UnsupportedSchema(u32),
    Identity(IdentityError),
    EmptyName,
    InvalidVersion(String),
    InvalidSourceRoot(String),
    InvalidCapability(String),
    DuplicateCapability(String),
    InvalidEntitlement,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(error) => write!(formatter, "invalid package manifest: {error}"),
            Self::UnsupportedSchema(schema) => {
                write!(
                    formatter,
                    "unsupported package manifest schema {schema}; expected 2"
                )
            }
            Self::Identity(error) => error.fmt(formatter),
            Self::EmptyName => formatter.write_str("package name cannot be empty"),
            Self::InvalidVersion(error) => write!(formatter, "invalid semantic version: {error}"),
            Self::InvalidSourceRoot(root) => {
                write!(formatter, "source_root must be \"src\", found {root:?}")
            }
            Self::InvalidCapability(capability) => {
                write!(formatter, "invalid capability id: {capability:?}")
            }
            Self::DuplicateCapability(capability) => {
                write!(formatter, "duplicate capability: {capability}")
            }
            Self::InvalidEntitlement => formatter.write_str("entitlement cannot be empty"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLICATION: &str = r#"
schema = 2
kind = "application"
id = "builtin.classic-rules"
name = "Classic Rules"
version = "1.0.0"
source_root = "src"
entry = "snake.classic_rules"
activation = "default-enabled"
priority = 400
handler_fuel = 20000
capabilities = ["diagnostics.log"]

[dependencies]
snake_common = { path = "../snake-common" }
"#;

    #[test]
    fn parses_strict_application_schema() {
        let manifest = PackageManifest::parse(APPLICATION).unwrap();
        assert!(manifest.is_application());
        assert_eq!(
            manifest.expected_entry_source().unwrap().as_str(),
            "src/snake/classic_rules.nexa"
        );
        assert_eq!(manifest.dependencies.len(), 1);
    }

    #[test]
    fn rejects_schema_one_and_unknown_fields() {
        assert!(matches!(
            PackageManifest::parse(&APPLICATION.replace("schema = 2", "schema = 1")),
            Err(ManifestError::UnsupportedSchema(1))
        ));
        let unknown =
            APPLICATION.replace("priority = 400", "priority = 400\nstate_schema = \"v1\"");
        assert!(matches!(
            PackageManifest::parse(&unknown),
            Err(ManifestError::Toml(_))
        ));
    }

    #[test]
    fn library_rejects_application_fields() {
        let valid = r#"
schema = 2
kind = "library"
id = "shared.snake-common"
name = "Snake Common"
version = "1.0.0"
source_root = "src"
"#;
        assert!(PackageManifest::parse(valid).unwrap().is_library());
        let invalid = format!("{valid}\nentry = \"main\"\n");
        assert!(matches!(
            PackageManifest::parse(&invalid),
            Err(ManifestError::Toml(_))
        ));
    }

    #[test]
    fn semver_is_not_approximated() {
        let invalid = APPLICATION.replace("1.0.0", "version-ish");
        assert!(matches!(
            PackageManifest::parse(&invalid),
            Err(ManifestError::InvalidVersion(_))
        ));
    }
}
