use std::fmt;
use std::fmt::Write as _;

/// Semantic version of the compiler-provided standard library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StandardLibraryVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl StandardLibraryVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for StandardLibraryVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Canonical source-level symbol kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    Function,
    Type,
}

impl SymbolKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Type => "type",
        }
    }
}

/// A field in a compiler-provided nominal type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FieldDescriptor {
    pub name: &'static str,
    pub ty: &'static str,
}

impl FieldDescriptor {
    #[must_use]
    pub const fn new(name: &'static str, ty: &'static str) -> Self {
        Self { name, ty }
    }
}

/// Kind of a compiler-provided nominal type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKind {
    Struct,
}

impl TypeKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Struct => "struct",
        }
    }
}

/// Deterministic nominal type metadata consumed by compiler resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeDescriptor {
    pub name: &'static str,
    pub kind: TypeKind,
    pub fields: &'static [FieldDescriptor],
    pub contract: &'static str,
}

impl TypeDescriptor {
    #[must_use]
    pub fn canonical_declaration(&self) -> String {
        let mut output = String::from("pub ");
        output.push_str(self.kind.as_str());
        output.push(' ');
        output.push_str(self.name);
        output.push('{');
        for (index, field) in self.fields.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str(field.name);
            output.push(':');
            output.push_str(field.ty);
        }
        output.push('}');
        output
    }
}

/// A function parameter in compiler syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParameterDescriptor {
    pub name: &'static str,
    pub ty: &'static str,
}

impl ParameterDescriptor {
    #[must_use]
    pub const fn new(name: &'static str, ty: &'static str) -> Self {
        Self { name, ty }
    }
}

/// Compiler lowering for a standard-library declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lowering {
    /// Parse and statically link the declaration from the module's embedded
    /// Nexa source.
    EmbeddedSource,
    /// Resolve the declaration synthetically and lower it using the named
    /// compiler intrinsic.
    CompilerIntrinsic(Intrinsic),
}

impl Lowering {
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::EmbeddedSource => "source",
            Self::CompilerIntrinsic(intrinsic) => intrinsic.canonical_name(),
        }
    }
}

/// Stable intrinsic names consumed by compiler resolution and lowering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Intrinsic {
    OptionIsSome,
    OptionIsNone,
    ResultIsOk,
    ResultIsErr,
    OptionUnwrapOr,
    ResultUnwrapOr,
    StringToString,
    I32ToString,
    I64ToString,
    F32ToString,
    F64ToString,
    BoolToString,
    RuneToString,
    StringLen,
    StringByteLen,
    StringContains,
    StringStartsWith,
    StringEndsWith,
    StringSubstring,
    StringTrim,
    StringSplit,
    F32Floor,
    F64Floor,
    F32Ceil,
    F64Ceil,
    F32Round,
    F64Round,
    F32Sqrt,
    F64Sqrt,
    F32Sin,
    F64Sin,
    F32Cos,
    F64Cos,
    ArrayLen,
    ArrayIsEmpty,
    ArrayGet,
    ArrayPush,
    ArrayPop,
    ArrayReserve,
    ArrayCapacity,
    ArrayClear,
    ArrayShrinkToFit,
    MapLen,
    MapContains,
    MapGet,
    MapInsert,
    MapRemove,
    DebugAssert,
    DebugTrap,
}

impl Intrinsic {
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::OptionIsSome => "intrinsic.option.is_some.v1",
            Self::OptionIsNone => "intrinsic.option.is_none.v1",
            Self::ResultIsOk => "intrinsic.result.is_ok.v1",
            Self::ResultIsErr => "intrinsic.result.is_err.v1",
            Self::OptionUnwrapOr => "intrinsic.option.unwrap_or.v1",
            Self::ResultUnwrapOr => "intrinsic.result.unwrap_or.v1",
            Self::StringToString => "intrinsic.scalar.string_to_string.v1",
            Self::I32ToString => "intrinsic.scalar.i32_to_string.v1",
            Self::I64ToString => "intrinsic.scalar.i64_to_string.v1",
            Self::F32ToString => "intrinsic.scalar.f32_to_string.v1",
            Self::F64ToString => "intrinsic.scalar.f64_to_string.v1",
            Self::BoolToString => "intrinsic.scalar.bool_to_string.v1",
            Self::RuneToString => "intrinsic.scalar.rune_to_string.v1",
            Self::StringLen => "intrinsic.string.len_scalar.v1",
            Self::StringByteLen => "intrinsic.string.byte_len_utf8.v1",
            Self::StringContains => "intrinsic.string.contains.v1",
            Self::StringStartsWith => "intrinsic.string.starts_with.v1",
            Self::StringEndsWith => "intrinsic.string.ends_with.v1",
            Self::StringSubstring => "intrinsic.string.substring_scalar.v1",
            Self::StringTrim => "intrinsic.string.trim_unicode.v1",
            Self::StringSplit => "intrinsic.string.split_exact.v1",
            Self::F32Floor => "intrinsic.math.f32.floor.v1",
            Self::F64Floor => "intrinsic.math.f64.floor.v1",
            Self::F32Ceil => "intrinsic.math.f32.ceil.v1",
            Self::F64Ceil => "intrinsic.math.f64.ceil.v1",
            Self::F32Round => "intrinsic.math.f32.round.v1",
            Self::F64Round => "intrinsic.math.f64.round.v1",
            Self::F32Sqrt => "intrinsic.math.f32.sqrt.v1",
            Self::F64Sqrt => "intrinsic.math.f64.sqrt.v1",
            Self::F32Sin => "intrinsic.math.f32.sin.v1",
            Self::F64Sin => "intrinsic.math.f64.sin.v1",
            Self::F32Cos => "intrinsic.math.f32.cos.v1",
            Self::F64Cos => "intrinsic.math.f64.cos.v1",
            Self::ArrayLen => "intrinsic.array.len.v1",
            Self::ArrayIsEmpty => "intrinsic.array.is_empty.v1",
            Self::ArrayGet => "intrinsic.array.get.v1",
            Self::ArrayPush => "intrinsic.array.push.v1",
            Self::ArrayPop => "intrinsic.array.pop.v1",
            Self::ArrayReserve => "intrinsic.array.reserve.v1",
            Self::ArrayCapacity => "intrinsic.array.capacity.v1",
            Self::ArrayClear => "intrinsic.array.clear.v1",
            Self::ArrayShrinkToFit => "intrinsic.array.shrink_to_fit.v1",
            Self::MapLen => "intrinsic.map.len.v1",
            Self::MapContains => "intrinsic.map.contains.v1",
            Self::MapGet => "intrinsic.map.get.v1",
            Self::MapInsert => "intrinsic.map.insert.v1",
            Self::MapRemove => "intrinsic.map.remove.v1",
            Self::DebugAssert => "intrinsic.debug.assert.v1",
            Self::DebugTrap => "intrinsic.debug.trap.v1",
        }
    }
}

/// Language-visible effect. M4 standard-library functions are all pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    Pure,
    LocalMutation,
}

impl Effect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::LocalMutation => "local-mutation",
        }
    }
}

/// Whether evaluation can allocate a language value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Allocation {
    None,
    Result,
}

impl Allocation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Result => "result",
        }
    }
}

/// Termination behavior independent of host side effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Termination {
    Returns,
    MayTrap,
    AlwaysTraps,
}

impl Termination {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Returns => "returns",
            Self::MayTrap => "may-trap",
            Self::AlwaysTraps => "always-traps",
        }
    }
}

/// Determinism, allocation, and termination contract for a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FunctionBehavior {
    pub effect: Effect,
    pub allocation: Allocation,
    pub termination: Termination,
}

impl FunctionBehavior {
    pub const TOTAL: Self = Self::new(Allocation::None, Termination::Returns);
    pub const ALLOCATES: Self = Self::new(Allocation::Result, Termination::Returns);
    pub const MAY_TRAP: Self = Self::new(Allocation::None, Termination::MayTrap);
    pub const ALLOCATES_OR_TRAPS: Self = Self::new(Allocation::Result, Termination::MayTrap);
    pub const ALWAYS_TRAPS: Self = Self::new(Allocation::None, Termination::AlwaysTraps);
    pub const MUTATES: Self = Self::local_mutation(Allocation::None, Termination::Returns);
    pub const MUTATES_OR_TRAPS: Self = Self::local_mutation(Allocation::None, Termination::MayTrap);
    pub const MUTATES_AND_ALLOCATES: Self =
        Self::local_mutation(Allocation::Result, Termination::Returns);
    pub const MUTATES_ALLOCATES_OR_TRAPS: Self =
        Self::local_mutation(Allocation::Result, Termination::MayTrap);

    #[must_use]
    pub const fn new(allocation: Allocation, termination: Termination) -> Self {
        Self {
            effect: Effect::Pure,
            allocation,
            termination,
        }
    }

    #[must_use]
    pub const fn local_mutation(allocation: Allocation, termination: Termination) -> Self {
        Self {
            effect: Effect::LocalMutation,
            allocation,
            termination,
        }
    }
}

/// Deterministic declaration metadata consumed by compiler resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FunctionDescriptor {
    pub name: &'static str,
    pub type_parameters: &'static [&'static str],
    pub parameters: &'static [ParameterDescriptor],
    pub result: &'static str,
    pub lowering: Lowering,
    pub behavior: FunctionBehavior,
    /// Concise normative semantics, including units where they matter.
    pub contract: &'static str,
}

impl FunctionDescriptor {
    /// Renders a stable declaration shape. Synthetic generic declarations are
    /// metadata and are not inserted into embedded source.
    #[must_use]
    pub fn canonical_declaration(&self) -> String {
        let mut output = String::from("pub fn ");
        output.push_str(self.name);
        if !self.type_parameters.is_empty() {
            output.push('<');
            for (index, parameter) in self.type_parameters.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(parameter);
            }
            output.push('>');
        }
        output.push('(');
        for (index, parameter) in self.parameters.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str(parameter.name);
            output.push(':');
            output.push_str(parameter.ty);
        }
        output.push_str(")->");
        output.push_str(self.result);
        output
    }
}

/// One compiler-provided module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModuleDescriptor {
    /// Short lookup name, such as `math`.
    pub name: &'static str,
    /// Canonical internal module path, such as `std.math`. Source-level paths
    /// render the same segments as `std::math`.
    pub path: &'static str,
    /// Whether names from this module are eligible for implicit prelude
    /// resolution.
    pub prelude: bool,
    /// Nexa source for all declarations with `EmbeddedSource` lowering.
    pub source: &'static str,
    pub types: &'static [TypeDescriptor],
    pub functions: &'static [FunctionDescriptor],
}

impl ModuleDescriptor {
    #[must_use]
    pub fn ty(&self, name: &str) -> Option<&'static TypeDescriptor> {
        self.types.iter().find(|ty| ty.name == name)
    }

    #[must_use]
    pub fn function(&self, name: &str) -> Option<&'static FunctionDescriptor> {
        self.functions.iter().find(|function| function.name == name)
    }

    #[must_use]
    pub fn canonical_symbol(&self, function: &FunctionDescriptor) -> CanonicalSymbol {
        CanonicalSymbol {
            package_id: crate::CANONICAL_PACKAGE_ID,
            module_path: self.path,
            kind: SymbolKind::Function,
            name: function.name,
        }
    }

    #[must_use]
    pub fn canonical_type_symbol(&self, ty: &TypeDescriptor) -> CanonicalSymbol {
        CanonicalSymbol {
            package_id: crate::CANONICAL_PACKAGE_ID,
            module_path: self.path,
            kind: SymbolKind::Type,
            name: ty.name,
        }
    }
}

/// Exact, non-hashed identity that the compiler retains alongside any compact
/// runtime identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalSymbol {
    pub package_id: &'static str,
    pub module_path: &'static str,
    pub kind: SymbolKind,
    pub name: &'static str,
}

impl CanonicalSymbol {
    /// Renders an unambiguous, versioned canonical name.
    #[must_use]
    pub fn canonical_name(self) -> String {
        format!(
            "{}::{}::{}::{}",
            self.package_id,
            self.module_path,
            self.kind.as_str(),
            self.name
        )
    }
}

/// Stable, non-cryptographic hash of the length-framed canonical manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DescriptorHash(pub u64);

impl fmt::Display for DescriptorHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

/// Entire compiler-provided standard-library model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StandardLibrary {
    pub descriptor_schema: u16,
    pub package_id: &'static str,
    pub canonical_package_id: &'static str,
    pub version: StandardLibraryVersion,
    modules: &'static [ModuleDescriptor],
}

impl StandardLibrary {
    #[must_use]
    pub const fn new(
        descriptor_schema: u16,
        package_id: &'static str,
        canonical_package_id: &'static str,
        version: StandardLibraryVersion,
        modules: &'static [ModuleDescriptor],
    ) -> Self {
        Self {
            descriptor_schema,
            package_id,
            canonical_package_id,
            version,
            modules,
        }
    }

    #[must_use]
    pub const fn modules(&self) -> &'static [ModuleDescriptor] {
        self.modules
    }

    /// Accepts either a short name (`string`) or a source-level reserved path
    /// (`std::string`).
    #[must_use]
    pub fn module(&self, name_or_path: &str) -> Option<&'static ModuleDescriptor> {
        self.modules.iter().find(|module| {
            module.name == name_or_path || module.path.split('.').eq(name_or_path.split("::"))
        })
    }

    /// Resolves `core::min_i32` or `std::core::min_i32`.
    #[must_use]
    pub fn function(
        &self,
        qualified_name: &str,
    ) -> Option<(&'static ModuleDescriptor, &'static FunctionDescriptor)> {
        let (qualifier, name) = qualified_name.rsplit_once("::")?;
        self.modules.iter().find_map(|module| {
            (module.name == qualifier || module.path.split('.').eq(qualifier.split("::")))
                .then(|| module.function(name))
                .flatten()
                .map(|function| (module, function))
        })
    }

    /// Resolves `math::Vec2` or `std::math::Vec2`.
    #[must_use]
    pub fn ty(
        &self,
        qualified_name: &str,
    ) -> Option<(&'static ModuleDescriptor, &'static TypeDescriptor)> {
        let (qualifier, name) = qualified_name.rsplit_once("::")?;
        self.modules.iter().find_map(|module| {
            (module.name == qualifier || module.path.split('.').eq(qualifier.split("::")))
                .then(|| module.ty(name))
                .flatten()
                .map(|ty| (module, ty))
        })
    }

    /// Iterates symbols in the fixed module/declaration order of this schema.
    pub fn symbols(&self) -> impl Iterator<Item = CanonicalSymbol> + '_ {
        self.modules.iter().flat_map(|module| {
            let types = module
                .types
                .iter()
                .map(|ty| module.canonical_type_symbol(ty));
            let functions = module
                .functions
                .iter()
                .map(|function| module.canonical_symbol(function));
            types.chain(functions)
        })
    }

    /// Length-framed canonical descriptor text for build fingerprints and
    /// deterministic cache keys.
    #[must_use]
    pub fn canonical_manifest(&self) -> String {
        let mut output = String::new();
        push_field(&mut output, "schema", &self.descriptor_schema.to_string());
        push_field(&mut output, "package", self.package_id);
        push_field(&mut output, "canonical-package", self.canonical_package_id);
        push_field(&mut output, "version", &self.version.to_string());
        for module in self.modules {
            push_field(&mut output, "module", module.path);
            push_field(
                &mut output,
                "prelude",
                if module.prelude { "true" } else { "false" },
            );
            push_field(&mut output, "source", module.source);
            for ty in module.types {
                let symbol = module.canonical_type_symbol(ty);
                push_field(&mut output, "symbol", &symbol.canonical_name());
                push_field(&mut output, "declaration", &ty.canonical_declaration());
                push_field(&mut output, "contract", ty.contract);
            }
            for function in module.functions {
                let symbol = module.canonical_symbol(function);
                push_field(&mut output, "symbol", &symbol.canonical_name());
                push_field(
                    &mut output,
                    "declaration",
                    &function.canonical_declaration(),
                );
                push_field(&mut output, "lowering", function.lowering.canonical_name());
                push_field(&mut output, "effect", function.behavior.effect.as_str());
                push_field(
                    &mut output,
                    "allocation",
                    function.behavior.allocation.as_str(),
                );
                push_field(
                    &mut output,
                    "termination",
                    function.behavior.termination.as_str(),
                );
                push_field(&mut output, "contract", function.contract);
            }
        }
        output
    }

    #[must_use]
    pub fn descriptor_hash(&self) -> DescriptorHash {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in self.canonical_manifest().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        DescriptorHash(hash)
    }
}

fn push_field(output: &mut String, name: &str, value: &str) {
    writeln!(output, "{}:{}:{}", name, value.len(), value).expect("writing to String cannot fail");
}
