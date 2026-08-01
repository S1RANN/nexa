use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use nexa_syntax::ast::{DeclarationKind, Visibility, parse_nexa_ast};
use nexa_syntax::parse_nexa;

use crate::{ModulePath, PackageSourceSet, SourceKey, SourceRole};

/// The exact NIDL entrypoint subset that can affect one package build.
///
/// All vectors are sorted and duplicate-free. Required entrypoints always participate even when
/// missing so changing an Engine requirement invalidates the candidate. Optional entrypoints
/// participate only when the package entry module actually implements a public function with the
/// declared name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveEntrypointSet {
    pub required: Vec<String>,
    pub implemented_optional: Vec<String>,
    pub effective: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectiveEntrypointScanError {
    RequiredEntrypointNotDeclared(String),
    InvalidSourceModule { source: SourceKey, message: String },
    SourceTooLarge(SourceKey),
}

impl fmt::Display for EffectiveEntrypointScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredEntrypointNotDeclared(name) => write!(
                formatter,
                "required entrypoint `{name}` is not declared by the Host contract"
            ),
            Self::InvalidSourceModule { source, message } => {
                write!(
                    formatter,
                    "cannot derive module for `{}/{}`: {message}",
                    source.package_id, source.path
                )
            }
            Self::SourceTooLarge(source) => {
                write!(
                    formatter,
                    "entry module source `{}/{}` exceeds the syntax limit",
                    source.package_id, source.path
                )
            }
        }
    }
}

impl Error for EffectiveEntrypointScanError {}

/// Pre-scans public function names in the root entry module before build fingerprinting.
///
/// This intentionally does not validate types or effects. If a declared optional entrypoint is
/// present with the wrong signature, its name still enters `effective`; the normal analyzer then
/// emits the precise signature diagnostic. Syntax recovery is likewise conservative and keeps
/// every public function declaration the parser can recover.
pub fn effective_entrypoint_set(
    root_sources: &PackageSourceSet,
    entry_module: &ModulePath,
    declared: &[String],
    required: &[String],
) -> Result<EffectiveEntrypointSet, EffectiveEntrypointScanError> {
    let declared = declared.iter().cloned().collect::<BTreeSet<_>>();
    let required = required.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(name) = required
        .iter()
        .find(|name| !declared.contains(name.as_str()))
    {
        return Err(EffectiveEntrypointScanError::RequiredEntrypointNotDeclared(
            name.clone(),
        ));
    }

    let mut public_functions = BTreeSet::new();
    for unit in root_sources
        .units()
        .values()
        .filter(|unit| unit.role == SourceRole::Production)
    {
        let module = unit.expected_module_path().map_err(|error| {
            EffectiveEntrypointScanError::InvalidSourceModule {
                source: unit.key.clone(),
                message: error.to_string(),
            }
        })?;
        if &module != entry_module {
            continue;
        }
        let syntax = parse_nexa(&unit.text)
            .map_err(|_| EffectiveEntrypointScanError::SourceTooLarge(unit.key.clone()))?;
        let ast = parse_nexa_ast(&syntax);
        public_functions.extend(ast.declarations.into_iter().filter_map(|declaration| {
            if declaration.visibility != Visibility::Public {
                return None;
            }
            let DeclarationKind::Function(function) = declaration.kind else {
                return None;
            };
            Some(function.name.text)
        }));
    }

    let implemented_optional = declared
        .difference(&required)
        .filter(|name| public_functions.contains(name.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let effective = required
        .union(&implemented_optional)
        .cloned()
        .collect::<Vec<_>>();
    Ok(EffectiveEntrypointSet {
        required: required.into_iter().collect(),
        implemented_optional: implemented_optional.into_iter().collect(),
        effective,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{CompilationLimits, NormalizedPackagePath, PackageId, SourceSetBuilder};

    use super::*;

    fn sources(text: &str) -> PackageSourceSet {
        let package = PackageId::new("example.entrypoints").unwrap();
        let mut builder = SourceSetBuilder::new(package, CompilationLimits::default());
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                Arc::<str>::from(text),
                SourceRole::Production,
            )
            .unwrap();
        builder.build().unwrap()
    }

    #[test]
    fn only_required_and_implemented_public_optional_entrypoints_are_effective() {
        let result = effective_entrypoint_set(
            &sources(
                "pub fn required() {}\n\
                 pub fn optional() {}\n\
                 fn private_optional() {}\n",
            ),
            &ModulePath::new("main").unwrap(),
            &[
                "private_optional".into(),
                "required".into(),
                "unused".into(),
                "optional".into(),
            ],
            &["required".into()],
        )
        .unwrap();
        assert_eq!(result.required, ["required"]);
        assert_eq!(result.implemented_optional, ["optional"]);
        assert_eq!(result.effective, ["optional", "required"]);
    }

    #[test]
    fn malformed_optional_signature_still_affects_the_fingerprint_set() {
        let result = effective_entrypoint_set(
            &sources("pub fn optional(value: Missing) -> unit {}\n"),
            &ModulePath::new("main").unwrap(),
            &["optional".into()],
            &[],
        )
        .unwrap();
        assert_eq!(result.effective, ["optional"]);
    }

    #[test]
    fn required_entrypoint_must_be_declared_by_the_contract() {
        let error = effective_entrypoint_set(
            &sources(""),
            &ModulePath::new("main").unwrap(),
            &[],
            &["missing".into()],
        )
        .unwrap_err();
        assert_eq!(
            error,
            EffectiveEntrypointScanError::RequiredEntrypointNotDeclared("missing".into())
        );
    }
}
