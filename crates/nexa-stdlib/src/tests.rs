use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::{
    CANONICAL_PACKAGE_ID, DESCRIPTOR_SCHEMA_VERSION, Effect, Intrinsic, Lowering, PACKAGE_ID,
    Termination, VERSION, buffer, canonical_descriptor_identity, collections, core as std_core,
    debug, math, set, standard_library, string,
};

#[test]
fn versioned_module_set_is_fixed_and_resolvable() {
    let library = standard_library();
    assert_eq!(library.descriptor_schema, DESCRIPTOR_SCHEMA_VERSION);
    assert_eq!(library.package_id, PACKAGE_ID);
    assert_eq!(library.canonical_package_id, CANONICAL_PACKAGE_ID);
    assert_eq!(library.version, VERSION);
    assert_eq!(
        library
            .modules()
            .iter()
            .map(|module| (module.name, module.path))
            .collect::<Vec<_>>(),
        [
            ("core", "std.core"),
            ("math", "std.math"),
            ("string", "std.string"),
            ("collections", "std.collections"),
            ("set", "std.set"),
            ("buffer", "std.buffer"),
            ("debug", "std.debug"),
        ]
    );
    assert!(library.module("string").is_some());
    assert!(library.module("std::string").is_some());
    assert!(library.module("std.string").is_none());
    assert!(library.module("missing").is_none());
}

#[test]
#[allow(clippy::too_many_lines)]
fn mandatory_api_catalog_and_canonical_descriptor_are_complete() {
    let library = standard_library();
    let expected_modules: &[(&str, &[&str], &[&str])] = &[
        (
            "core",
            &[],
            &[
                "is_some",
                "is_none",
                "is_ok",
                "is_err",
                "option_unwrap_or",
                "result_unwrap_or",
                "min_i32",
                "max_i32",
                "min_i64",
                "max_i64",
                "min_f32",
                "max_f32",
                "min_f64",
                "max_f64",
                "to_string_string",
                "to_string_i32",
                "to_string_i64",
                "to_string_f32",
                "to_string_f64",
                "to_string_bool",
                "to_string_rune",
            ],
        ),
        (
            "math",
            &["Vec2", "Vec3"],
            &[
                "clamp_i32",
                "abs_i32",
                "clamp_i64",
                "abs_i64",
                "clamp_f32",
                "abs_f32",
                "clamp_f64",
                "abs_f64",
                "floor_f32",
                "floor_f64",
                "ceil_f32",
                "ceil_f64",
                "round_f32",
                "round_f64",
                "sqrt_f32",
                "sqrt_f64",
                "sin_f32",
                "sin_f64",
                "cos_f32",
                "cos_f64",
                "vec2",
                "vec2_add",
                "vec2_sub",
                "vec2_scale",
                "vec2_dot",
                "vec2_length",
                "vec3",
                "vec3_add",
                "vec3_sub",
                "vec3_scale",
                "vec3_dot",
                "vec3_length",
            ],
        ),
        (
            "string",
            &[],
            &[
                "len",
                "byte_len",
                "contains",
                "starts_with",
                "ends_with",
                "substring",
                "trim",
                "split",
            ],
        ),
        (
            "collections",
            &[],
            &[
                "array_len",
                "array_is_empty",
                "array_get",
                "array_push",
                "array_pop",
                "array_reserve",
                "array_capacity",
                "array_clear",
                "array_shrink_to_fit",
                "map_len",
                "map_contains",
                "map_get",
                "map_insert",
                "map_remove",
                "array_first",
                "array_last",
                "array_swap",
                "array_reverse",
                "map_is_empty",
                "map_get_or",
                "map_insert_if_absent",
            ],
        ),
        (
            "set",
            &["Set"],
            &[
                "set_new",
                "set_len",
                "set_contains",
                "set_insert",
                "set_remove",
                "set_clear",
            ],
        ),
        ("buffer", &["Buffer"], &["buffer_is_empty", "buffer_fill"]),
        ("debug", &[], &["assert", "trap"]),
    ];

    assert_eq!(library.modules().len(), expected_modules.len());
    assert!(
        library.modules().iter().all(|module| !module.prelude),
        "every stdlib module must require an explicit namespace use"
    );
    for (module, (expected_name, expected_types, expected_functions)) in
        library.modules().iter().zip(expected_modules)
    {
        assert_eq!(module.name, *expected_name);
        assert_eq!(
            module.types.iter().map(|ty| ty.name).collect::<Vec<_>>(),
            *expected_types,
            "{} type catalog changed",
            module.path
        );
        assert_eq!(
            module
                .functions
                .iter()
                .map(|function| function.name)
                .collect::<Vec<_>>(),
            *expected_functions,
            "{} function catalog changed",
            module.path
        );
    }

    let required_intrinsics = [
        ("core::is_ok", Intrinsic::ResultIsOk),
        ("core::is_err", Intrinsic::ResultIsErr),
        ("core::option_unwrap_or", Intrinsic::OptionUnwrapOr),
        ("core::result_unwrap_or", Intrinsic::ResultUnwrapOr),
        ("core::to_string_string", Intrinsic::StringToString),
        ("core::to_string_i32", Intrinsic::I32ToString),
        ("core::to_string_i64", Intrinsic::I64ToString),
        ("core::to_string_f32", Intrinsic::F32ToString),
        ("core::to_string_f64", Intrinsic::F64ToString),
        ("core::to_string_bool", Intrinsic::BoolToString),
        ("core::to_string_rune", Intrinsic::RuneToString),
        ("math::floor_f32", Intrinsic::F32Floor),
        ("math::floor_f64", Intrinsic::F64Floor),
        ("math::ceil_f32", Intrinsic::F32Ceil),
        ("math::ceil_f64", Intrinsic::F64Ceil),
        ("math::round_f32", Intrinsic::F32Round),
        ("math::round_f64", Intrinsic::F64Round),
        ("math::sqrt_f32", Intrinsic::F32Sqrt),
        ("math::sqrt_f64", Intrinsic::F64Sqrt),
        ("math::sin_f32", Intrinsic::F32Sin),
        ("math::sin_f64", Intrinsic::F64Sin),
        ("math::cos_f32", Intrinsic::F32Cos),
        ("math::cos_f64", Intrinsic::F64Cos),
        ("string::len", Intrinsic::StringLen),
        ("string::byte_len", Intrinsic::StringByteLen),
        ("string::trim", Intrinsic::StringTrim),
        ("string::split", Intrinsic::StringSplit),
        ("collections::array_push", Intrinsic::ArrayPush),
        ("collections::array_pop", Intrinsic::ArrayPop),
        ("collections::array_reserve", Intrinsic::ArrayReserve),
        ("collections::array_capacity", Intrinsic::ArrayCapacity),
        ("collections::array_clear", Intrinsic::ArrayClear),
        (
            "collections::array_shrink_to_fit",
            Intrinsic::ArrayShrinkToFit,
        ),
        ("collections::map_insert", Intrinsic::MapInsert),
        ("collections::map_remove", Intrinsic::MapRemove),
        ("collections::map_is_empty", Intrinsic::MapIsEmpty),
        ("collections::map_get_or", Intrinsic::MapGetOr),
        (
            "collections::map_insert_if_absent",
            Intrinsic::MapInsertIfAbsent,
        ),
        ("collections::array_first", Intrinsic::ArrayFirst),
        ("collections::array_last", Intrinsic::ArrayLast),
        ("collections::array_swap", Intrinsic::ArraySwap),
        ("collections::array_reverse", Intrinsic::ArrayReverse),
        ("set::set_new", Intrinsic::SetNew),
        ("set::set_len", Intrinsic::SetLen),
        ("set::set_contains", Intrinsic::SetContains),
        ("set::set_insert", Intrinsic::SetInsert),
        ("set::set_remove", Intrinsic::SetRemove),
        ("set::set_clear", Intrinsic::SetClear),
        ("buffer::buffer_is_empty", Intrinsic::BufferIsEmpty),
        ("buffer::buffer_fill", Intrinsic::BufferFill),
        ("debug::assert", Intrinsic::DebugAssert),
        ("debug::trap", Intrinsic::DebugTrap),
    ];
    for (qualified_name, expected_intrinsic) in required_intrinsics {
        let (_, function) = library
            .function(qualified_name)
            .unwrap_or_else(|| panic!("missing mandatory symbol {qualified_name}"));
        assert_eq!(
            function.lowering,
            Lowering::CompilerIntrinsic(expected_intrinsic),
            "wrong lowering for {qualified_name}"
        );
    }

    let string_module = library.module("string").expect("string module");
    assert_eq!(
        string_module.function("len").expect("string.len").contract,
        "number of Unicode scalar values"
    );
    assert_eq!(
        string_module
            .function("byte_len")
            .expect("string.byte_len")
            .contract,
        "number of UTF-8 bytes"
    );
    assert!(
        string_module
            .function("substring")
            .expect("string.substring")
            .contract
            .contains("Unicode scalars")
    );
    let unit_probe = "界😀";
    assert_eq!(string::len(unit_probe), 2);
    assert_eq!(string::byte_len(unit_probe), 7);
    assert_eq!(string::substring(unit_probe, 1, 1).as_deref(), Ok("😀"));

    assert_eq!(library.descriptor_schema, 2);
    assert_eq!(library.package_id, "nexa.stdlib");
    assert_eq!(library.canonical_package_id, "nexa.stdlib@3.0.0");
    assert_eq!(library.version.to_string(), "3.0.0");
    let canonical = library.canonical_manifest();
    assert_eq!(canonical, library.canonical_manifest());
    assert_eq!(library.descriptor_hash().0, 0x326d_d3b4_4a73_aa90);
    assert_eq!(library.descriptor_hash(), library.descriptor_hash());
    assert_eq!(library.symbols().count(), 128);
    assert_eq!(library.methods().count(), 32);
    let abs = library.method("i32", "abs").expect("i32.abs method");
    assert_eq!(abs.implementation_module, "math");
    assert_eq!(abs.implementation, "abs_i32");
    assert_eq!(abs.canonical_declaration(), "pub method i32::abs()->i32");

    let canonical_lower = canonical.to_ascii_lowercase();
    for forbidden in [
        "use host::",
        "capability",
        "realm",
        "async fn",
        "log(",
        "print(",
    ] {
        assert!(
            !canonical_lower.contains(forbidden),
            "ambient authority leaked into stdlib descriptor: {forbidden}"
        );
    }
}

#[test]
fn canonical_symbols_are_unique_versioned_and_deterministic() {
    let library = standard_library();
    let first_manifest = library.canonical_manifest();
    let second_manifest = library.canonical_manifest();
    assert_eq!(first_manifest, second_manifest);
    assert_eq!(library.descriptor_hash(), library.descriptor_hash());
    assert_eq!(library.descriptor_hash().0, 0x326d_d3b4_4a73_aa90);

    let symbols = library
        .symbols()
        .map(super::model::CanonicalSymbol::canonical_name)
        .collect::<Vec<_>>();
    assert_eq!(symbols.len(), symbols.iter().collect::<BTreeSet<_>>().len());
    assert!(symbols.iter().all(|symbol| {
        symbol.starts_with("nexa.stdlib@3.0.0::")
            && (symbol.contains("::function::")
                || symbol.contains("::type::")
                || symbol.contains("::method::"))
    }));

    let (module, function) = library
        .function("std::math::clamp_i32")
        .expect("canonical standard function");
    assert_eq!(module.name, "math");
    assert_eq!(
        function.canonical_declaration(),
        "pub fn clamp_i32(value:i32,low:i32,high:i32)->i32"
    );
    let (module, ty) = library
        .ty("std::math::Vec2")
        .expect("canonical standard type");
    assert_eq!(module.name, "math");
    assert_eq!(ty.canonical_declaration(), "pub struct Vec2{x:f32,y:f32}");
}

#[test]
fn receiver_methods_match_their_internal_lowering_targets() {
    let library = standard_library();
    for method in library.methods() {
        let module = library
            .module(method.implementation_module)
            .unwrap_or_else(|| {
                panic!(
                    "missing method module for {}::{}",
                    method.receiver, method.name
                )
            });
        let function = module.function(method.implementation).unwrap_or_else(|| {
            panic!(
                "missing method target {}::{} for {}::{}",
                method.implementation_module, method.implementation, method.receiver, method.name
            )
        });
        assert_eq!(function.lowering, method.lowering);
        assert_eq!(function.behavior, method.behavior);
        assert_eq!(function.result, method.result);
        let Some((receiver, parameters)) = function.parameters.split_first() else {
            panic!(
                "method target {} has no receiver parameter",
                method.implementation
            );
        };
        assert_eq!(receiver.ty, method.receiver);
        assert_eq!(
            parameters
                .iter()
                .map(|parameter| parameter.ty)
                .collect::<Vec<_>>(),
            method
                .parameters
                .iter()
                .map(|parameter| parameter.ty)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn descriptor_identity_has_one_exact_length_framed_authority() {
    const PREFIX: &[u8] = b"nexa.stdlib.descriptor.v2\0";
    let library = standard_library();
    let manifest = library.canonical_manifest();
    let identity = canonical_descriptor_identity();
    let length_start = PREFIX.len();
    let manifest_start = length_start + std::mem::size_of::<u64>();
    let manifest_end = manifest_start + manifest.len();

    assert!(identity.starts_with(PREFIX));
    assert_eq!(
        &identity[length_start..manifest_start],
        &u64::try_from(manifest.len()).unwrap().to_le_bytes()
    );
    assert_eq!(&identity[manifest_start..manifest_end], manifest.as_bytes());
    assert_eq!(
        &identity[manifest_end..],
        &library.descriptor_hash().0.to_le_bytes()
    );
    assert_eq!(identity, canonical_descriptor_identity());
}

#[test]
fn descriptors_are_static_deterministic_and_have_no_ambient_authority() {
    let library = standard_library();
    for module in library.modules() {
        let source = module.source.to_ascii_lowercase();
        for forbidden in [
            "use host::",
            "capability",
            "realm",
            "async fn",
            "log(",
            "print(",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} source contains forbidden `{forbidden}`",
                module.path
            );
        }
        assert!(
            !source.trim_start().starts_with("module "),
            "{} source declares a module instead of deriving it from its path",
            module.path
        );
        assert!(module.functions.iter().all(|function| matches!(
            function.behavior.effect,
            Effect::Pure | Effect::LocalMutation
        )));
    }
}

#[test]
fn core_option_and_result_helpers_have_reference_and_intrinsic_models() {
    assert!(std_core::is_some(&Some(3)));
    assert!(std_core::is_none::<i32>(&None));
    assert!(std_core::is_ok(&Result::<i32, &str>::Ok(3)));
    assert!(std_core::is_err(&Result::<i32, &str>::Err("bad")));
    assert_eq!(std_core::option_unwrap_or(Some(3), 5), 3);
    assert_eq!(std_core::option_unwrap_or(None, 5), 5);
    assert_eq!(std_core::result_unwrap_or::<_, &str>(Ok(3), 5), 3);
    assert_eq!(std_core::result_unwrap_or(Err::<i32, _>("bad"), 5), 5);

    let library = standard_library();
    for (name, intrinsic) in [
        ("core::is_ok", Intrinsic::ResultIsOk),
        ("core::is_err", Intrinsic::ResultIsErr),
        ("core::option_unwrap_or", Intrinsic::OptionUnwrapOr),
        ("core::result_unwrap_or", Intrinsic::ResultUnwrapOr),
    ] {
        let (_, function) = library.function(name).expect("core descriptor");
        assert_eq!(function.lowering, Lowering::CompilerIntrinsic(intrinsic));
    }
}

#[test]
fn unicode_string_units_are_scalar_values_and_bytes_are_explicit() {
    let value = "a界😀e\u{301}";
    assert_eq!(string::len(value), 5);
    assert_eq!(string::len_i32(value), Ok(5));
    assert_eq!(string::byte_len(value), 11);
    assert_eq!(string::substring(value, 1, 3).as_deref(), Ok("界😀e"));
    assert_eq!(string::slice(value, 2, 5).as_deref(), Ok("😀e\u{301}"));
    assert!(string::contains(value, "😀e"));
    assert!(string::starts_with(value, "a界"));
    assert!(string::ends_with(value, "e\u{301}"));
}

#[test]
fn invalid_scalar_ranges_are_rejected_without_byte_slicing() {
    let value = "界😀";
    assert!(string::substring(value, 1, 2).is_err());
    assert!(string::substring_i32(value, -1, 1).is_err());
    assert!(string::substring_i32(value, 0, -1).is_err());
    assert_eq!(string::substring(value, 2, 0).as_deref(), Ok(""));
}

#[test]
fn scalar_to_string_trim_and_split_are_locale_free() {
    assert_eq!(std_core::to_string_string("界"), "界");
    assert_eq!(std_core::to_string_i32(-42), "-42");
    assert_eq!(std_core::to_string_i64(i64::MIN), "-9223372036854775808");
    assert_eq!(std_core::to_string_f32(7.5), "7.5");
    assert_eq!(std_core::to_string_f64(f64::INFINITY), "inf");
    assert_eq!(std_core::to_string_bool(true), "true");
    assert_eq!(std_core::to_string_rune('界'), "界");
    assert_eq!(string::trim("\u{2003} hello 界 \n"), "hello 界");
    assert_eq!(
        string::split("a界a界", "界"),
        ["a".to_owned(), "a".to_owned(), String::new()]
    );

    let library = standard_library();
    for (name, intrinsic) in [
        ("core::to_string_string", Intrinsic::StringToString),
        ("core::to_string_i32", Intrinsic::I32ToString),
        ("core::to_string_i64", Intrinsic::I64ToString),
        ("core::to_string_f32", Intrinsic::F32ToString),
        ("core::to_string_f64", Intrinsic::F64ToString),
        ("core::to_string_bool", Intrinsic::BoolToString),
        ("core::to_string_rune", Intrinsic::RuneToString),
        ("string::trim", Intrinsic::StringTrim),
        ("string::split", Intrinsic::StringSplit),
    ] {
        let (_, function) = library.function(name).expect("stdlib descriptor");
        assert_eq!(function.lowering, Lowering::CompilerIntrinsic(intrinsic));
    }
}

#[test]
fn numeric_reference_operations_match_source_contracts() {
    assert_eq!(std_core::min_i32(3, -4), -4);
    assert_eq!(std_core::max_i64(3, -4), 3);
    assert_eq!(math::clamp_i32(15, 1, 10), 10);
    assert_eq!(math::abs_i32(-9), Ok(9));
    assert!(math::abs_i32(i32::MIN).is_err());
    assert!(math::abs_i64(i64::MIN).is_err());
    assert!((std_core::min_f32(3.5, -1.0) - -1.0).abs() < f32::EPSILON);
    assert!((math::abs_f64(-0.5) - 0.5).abs() < f64::EPSILON);
}

#[test]
fn extended_math_and_vector_operations_have_catalog_entries() {
    assert!((math::floor_f32(2.75) - 2.0).abs() < f32::EPSILON);
    assert!((math::ceil_f64(2.25) - 3.0).abs() < f64::EPSILON);
    assert!((math::round_f32(-2.5) - -3.0).abs() < f32::EPSILON);
    assert!((math::sqrt_f64(81.0) - 9.0).abs() < f64::EPSILON);
    assert!(math::sin_f32(0.0).abs() < f32::EPSILON);
    assert!((math::cos_f64(0.0) - 1.0).abs() < f64::EPSILON);

    let first = math::vec2(3.0, 4.0);
    let second = math::vec2(1.0, 2.0);
    assert_eq!(math::vec2_add(first, second), math::Vec2::new(4.0, 6.0));
    assert_eq!(math::vec2_sub(first, second), math::Vec2::new(2.0, 2.0));
    assert_eq!(math::vec2_scale(second, 2.0), math::Vec2::new(2.0, 4.0));
    assert!((math::vec2_dot(first, second) - 11.0).abs() < f32::EPSILON);
    assert!((math::vec2_length(first) - 5.0).abs() < f32::EPSILON);

    let first = math::vec3(1.0, 2.0, 2.0);
    let second = math::vec3(2.0, 1.0, 0.0);
    assert_eq!(
        math::vec3_add(first, second),
        math::Vec3::new(3.0, 3.0, 2.0)
    );
    assert_eq!(
        math::vec3_sub(first, second),
        math::Vec3::new(-1.0, 1.0, 2.0)
    );
    assert_eq!(math::vec3_scale(first, 2.0), math::Vec3::new(2.0, 4.0, 4.0));
    assert!((math::vec3_dot(first, second) - 4.0).abs() < f32::EPSILON);
    assert!((math::vec3_length(first) - 3.0).abs() < f32::EPSILON);

    let library = standard_library();
    assert!(library.ty("math::Vec2").is_some());
    assert!(library.ty("math::Vec3").is_some());
    for name in [
        "floor_f32",
        "floor_f64",
        "ceil_f32",
        "ceil_f64",
        "round_f32",
        "round_f64",
        "sqrt_f32",
        "sqrt_f64",
        "sin_f32",
        "sin_f64",
        "cos_f32",
        "cos_f64",
        "vec2",
        "vec2_add",
        "vec2_sub",
        "vec2_scale",
        "vec2_dot",
        "vec2_length",
        "vec3",
        "vec3_add",
        "vec3_sub",
        "vec3_scale",
        "vec3_dot",
        "vec3_length",
    ] {
        assert!(
            library.function(&format!("math::{name}")).is_some(),
            "missing math descriptor {name}"
        );
    }
}

#[test]
fn math_reference_helpers_match_canonical_backend_bit_for_bit() {
    let f32_values = [
        f32::from_bits(0x8000_0000),
        f32::from_bits(1),
        -1.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::from_bits(0x7f80_0001),
        f32::from_bits(0xffc0_1234),
    ];
    for value in f32_values {
        assert_eq!(
            math::floor_f32(value).to_bits(),
            nexa_core::deterministic_math::floor_f32(value).to_bits()
        );
        assert_eq!(
            math::ceil_f32(value).to_bits(),
            nexa_core::deterministic_math::ceil_f32(value).to_bits()
        );
        assert_eq!(
            math::round_f32(value).to_bits(),
            nexa_core::deterministic_math::round_f32(value).to_bits()
        );
        assert_eq!(
            math::sqrt_f32(value).to_bits(),
            nexa_core::deterministic_math::sqrt_f32(value).to_bits()
        );
        assert_eq!(
            math::sin_f32(value).to_bits(),
            nexa_core::deterministic_math::sin_f32(value).to_bits()
        );
        assert_eq!(
            math::cos_f32(value).to_bits(),
            nexa_core::deterministic_math::cos_f32(value).to_bits()
        );
    }

    let f64_values = [
        f64::from_bits(0x8000_0000_0000_0000),
        f64::from_bits(1),
        -1.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::from_bits(0x7ff0_0000_0000_0001),
        f64::from_bits(0xfff8_0000_0000_1234),
    ];
    for value in f64_values {
        assert_eq!(
            math::floor_f64(value).to_bits(),
            nexa_core::deterministic_math::floor_f64(value).to_bits()
        );
        assert_eq!(
            math::ceil_f64(value).to_bits(),
            nexa_core::deterministic_math::ceil_f64(value).to_bits()
        );
        assert_eq!(
            math::round_f64(value).to_bits(),
            nexa_core::deterministic_math::round_f64(value).to_bits()
        );
        assert_eq!(
            math::sqrt_f64(value).to_bits(),
            nexa_core::deterministic_math::sqrt_f64(value).to_bits()
        );
        assert_eq!(
            math::sin_f64(value).to_bits(),
            nexa_core::deterministic_math::sin_f64(value).to_bits()
        );
        assert_eq!(
            math::cos_f64(value).to_bits(),
            nexa_core::deterministic_math::cos_f64(value).to_bits()
        );
    }
}

#[test]
fn collection_reference_helpers_cover_arrays_and_maps() {
    let values = [3, 7, 7];
    assert_eq!(collections::array_len(&values), 3);
    assert!(!collections::array_is_empty(&values));
    assert_eq!(collections::array_get(&values, 1), Some(&7));

    let map = BTreeMap::from([("alpha", 1), ("beta", 2)]);
    assert_eq!(collections::map_len(&map), 2);
    assert!(collections::map_contains(&map, &"beta"));
    assert_eq!(collections::map_get(&map, &"alpha"), Some(&1));

    let mut values = vec![1, 2];
    assert!(collections::array_push(&mut values, 3));
    assert_eq!(collections::array_pop(&mut values), Ok(3));
    assert_eq!(values, [1, 2]);
    assert!(collections::array_pop(&mut Vec::<i32>::new()).is_err());
    assert!(collections::array_reserve(&mut values, 32));
    assert!(collections::array_capacity(&values) >= 34);
    assert!(collections::array_clear(&mut values));
    assert!(values.is_empty());
    assert!(collections::array_shrink_to_fit(&mut values));
    assert_eq!(collections::array_capacity(&values), 0);

    let mut map = BTreeMap::new();
    assert!(collections::map_insert(&mut map, "key", 7));
    assert_eq!(collections::map_remove(&mut map, &"key"), Some(7));
    assert!(map.is_empty());

    let mut values = vec![1, 2, 3];
    assert_eq!(collections::array_first(&values), Some(&1));
    assert_eq!(collections::array_last(&values), Some(&3));
    assert!(collections::array_swap(&mut values, 0, 2).is_ok());
    assert_eq!(values, [3, 2, 1]);
    assert!(collections::array_reverse(&mut values));
    assert_eq!(values, [1, 2, 3]);
    assert_eq!(collections::array_first::<i32>(&[]), None);
    assert!(collections::array_swap(&mut [1], 0, 1).is_err());

    let mut map = BTreeMap::from([("a", 1), ("b", 2)]);
    assert!(!collections::map_is_empty(&map));
    assert_eq!(collections::map_get_or(&map, &"a", 0), 1);
    assert_eq!(collections::map_get_or(&map, &"z", 99), 99);
    assert!(collections::map_insert_if_absent(&mut map, "c", 3));
    assert!(!collections::map_insert_if_absent(&mut map, "a", 99));

    let mut set = HashSet::new();
    assert!(set::set_insert(&mut set, 1));
    assert!(set::set_insert(&mut set, 2));
    assert_eq!(set::set_len(&set), 2);
    assert!(set::set_contains(&set, &1));
    assert!(!set::set_contains(&set, &3));
    assert!(set::set_remove(&mut set, &1));
    assert!(!set::set_remove(&mut set, &1));
    set::set_clear(&mut set);
    assert_eq!(set::set_len(&set), 0);

    let mut buf = vec![0i32; 5];
    assert!(!buffer::buffer_is_empty(&buf));
    assert!(buffer::buffer_fill(&mut buf, 42));
    assert_eq!(buf, [42, 42, 42, 42, 42]);
    assert!(buffer::buffer_is_empty::<i32>(&[]));

    let library = standard_library();
    for (name, intrinsic) in [
        ("collections::array_push", Intrinsic::ArrayPush),
        ("collections::array_pop", Intrinsic::ArrayPop),
        ("collections::array_reserve", Intrinsic::ArrayReserve),
        ("collections::array_clear", Intrinsic::ArrayClear),
        (
            "collections::array_shrink_to_fit",
            Intrinsic::ArrayShrinkToFit,
        ),
        ("collections::map_insert", Intrinsic::MapInsert),
        ("collections::map_remove", Intrinsic::MapRemove),
        ("collections::array_swap", Intrinsic::ArraySwap),
        ("collections::array_reverse", Intrinsic::ArrayReverse),
        (
            "collections::map_insert_if_absent",
            Intrinsic::MapInsertIfAbsent,
        ),
        ("set::set_insert", Intrinsic::SetInsert),
        ("set::set_remove", Intrinsic::SetRemove),
        ("set::set_clear", Intrinsic::SetClear),
        ("buffer::buffer_fill", Intrinsic::BufferFill),
    ] {
        let (_, function) = library.function(name).expect("collection descriptor");
        assert_eq!(function.lowering, Lowering::CompilerIntrinsic(intrinsic));
        assert_eq!(function.behavior.effect, Effect::LocalMutation);
    }
}

#[test]
fn debug_operations_return_traps_as_data_and_never_log() {
    assert_eq!(debug::assert(true), Ok(true));
    let assertion = debug::assert(false).expect_err("false assertion");
    assert_eq!(assertion.kind, debug::TrapKind::AssertionFailed);
    assert_eq!(assertion.message, "assertion failed");

    let explicit = debug::trap("stop").expect_err("explicit trap");
    assert_eq!(explicit.kind, debug::TrapKind::Explicit);
    assert_eq!(explicit.message, "stop");

    let library = standard_library();
    let (_, trap) = library.function("debug::trap").expect("trap descriptor");
    assert_eq!(
        trap.lowering,
        Lowering::CompilerIntrinsic(Intrinsic::DebugTrap)
    );
    assert_eq!(trap.behavior.termination, Termination::AlwaysTraps);
}
