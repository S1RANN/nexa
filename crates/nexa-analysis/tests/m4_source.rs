use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageKind, PackageManifest, SourceRole,
    load_package_directory, validate_module_source_for_role,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempTree(PathBuf);

impl TempTree {
    fn new(name: &str) -> Self {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nexa-analysis-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, source: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

#[test]
fn schema_two_loader_keeps_production_tests_and_lock_separate() {
    let tree = TempTree::new("source");
    write(
        &tree.path().join("package.toml"),
        r#"
schema = 2
kind = "application"
id = "example.food"
name = "Food"
version = "1.2.3"
source_root = "src"
entry = "food.effects"
activation = "programmatic"
"#,
    );
    write(
        &tree.path().join("src/food/effects.nexa"),
        "/// Food effects\npub fn effect() -> i32 { return 1; }\n",
    );
    write(
        &tree.path().join("tests/food/effect_test.nexa"),
        "@test fn effect_works() -> bool { return true; }\n",
    );
    let loaded = load_package_directory(tree.path(), CompilationLimits::default()).unwrap();
    assert_eq!(loaded.manifest.kind, PackageKind::Application);
    assert_eq!(loaded.production_sources.production_units().count(), 1);
    assert_eq!(loaded.test_sources.test_units().count(), 1);
    assert!(loaded.lock.is_none());
}

#[test]
fn loader_derives_module_identity_from_the_source_path() {
    let tree = TempTree::new("path-derived-module");
    write(
        &tree.path().join("package.toml"),
        r#"
schema = 2
kind = "library"
id = "example.library"
name = "Library"
version = "1.0.0"
source_root = "src"
"#,
    );
    write(
        &tree.path().join("src/food/effects.nexa"),
        "pub fn effect() -> i32 { return 1; }\n",
    );
    let loaded = load_package_directory(tree.path(), CompilationLimits::default()).unwrap();
    let unit = loaded
        .production_sources
        .production_units()
        .next()
        .expect("source unit");
    assert_eq!(
        unit.expected_module_path().unwrap().as_str(),
        "food.effects"
    );
}

#[test]
fn loader_snapshots_invalid_test_syntax_for_deferred_test_analysis() {
    let tree = TempTree::new("deferred-test-analysis");
    write(
        &tree.path().join("package.toml"),
        r#"
schema = 2
kind = "application"
id = "example.deferred"
name = "Deferred Tests"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
    );
    write(
        &tree.path().join("src/main.nexa"),
        "pub fn value() -> i32 { return 1; }\n",
    );
    let invalid_test = "@test fn broken( -> bool { return true; }\n";
    write(&tree.path().join("tests/checks.nexa"), invalid_test);

    let loaded = load_package_directory(tree.path(), CompilationLimits::default())
        .expect("product discovery snapshots tests without validating their syntax");
    let test = loaded
        .test_sources
        .test_units()
        .next()
        .expect("invalid test source remains available to the test target");
    assert_eq!(test.text.as_ref(), invalid_test);
    assert_eq!(
        validate_module_source_for_role(
            &NormalizedPackagePath::new("tests/checks.nexa").unwrap(),
            invalid_test,
            SourceRole::Test,
        )
        .expect("syntax validation is deferred to the test target")
        .as_str(),
        "test.checks"
    );
}

#[test]
fn library_rejects_an_application_entry() {
    let library_with_entry = r#"
schema = 2
kind = "library"
id = "example.library"
name = "Library"
version = "1.0.0"
source_root = "src"
entry = "main"
"#;
    assert!(PackageManifest::parse(library_with_entry).is_err());
}

#[test]
fn library_rejects_an_application_activation_policy() {
    let library_with_activation = r#"
schema = 2
kind = "library"
id = "example.library"
name = "Library"
version = "1.0.0"
source_root = "src"
activation = "required"
"#;
    assert!(PackageManifest::parse(library_with_activation).is_err());
}

#[cfg(unix)]
#[test]
fn loader_rejects_source_symlinks() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new("symlink");
    write(
        &tree.path().join("package.toml"),
        r#"
schema = 2
kind = "library"
id = "example.library"
name = "Library"
version = "1.0.0"
source_root = "src"
"#,
    );
    write(
        &tree.path().join("outside.nexa"),
        "pub fn escaped() -> i32 { return 0; }\n",
    );
    fs::create_dir_all(tree.path().join("src")).unwrap();
    symlink(
        tree.path().join("outside.nexa"),
        tree.path().join("src/escaped.nexa"),
    )
    .unwrap();
    assert!(load_package_directory(tree.path(), CompilationLimits::default()).is_err());
}
