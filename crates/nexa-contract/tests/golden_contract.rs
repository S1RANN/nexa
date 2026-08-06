//! Phase 2 Golden baseline (task #5).
//!
//! Locks, for the checked-in `fixtures/golden.contract.nexa` source: the Contract Stable ID,
//! every declaration Stable ID (handle/struct/field/enum/variant/host fn/nexa fn/parameters),
//! the ABI descriptor bytes, the Contract fingerprint, the provenance header stamped from the
//! Contract source file name, and a canonical snapshot of the generated public binding API.
//! Semantic-equivalent migrations must not change any of these; the only permitted version
//! upgrade this milestone is `CONTRACT_SYNTAX_VERSION` (recorded in the Build Fingerprint via
//! `nexa-analysis::BuildFingerprintInput`, never in the descriptor bytes).

use nexa_contract::{abi_descriptor, generate_rust, generate_rust_for_source_file, parse_contract};

const GOLDEN_SOURCE: &str = include_str!("fixtures/golden.contract.nexa");
#[test]
fn golden_contract_and_declaration_stable_ids_are_locked() {
    let contract = parse_contract(GOLDEN_SOURCE)
        .expect("the Golden contract is valid flat Contract v3 syntax");

    assert_eq!(contract.stable_id.0, 0x079f_48e7_2477_dfb0, "contract");
    assert_eq!(contract.handles[0].stable_id.0, 0x5805_175c_3d8c_424c, "handle Entity");

    let cell = &contract.structs[0];
    assert_eq!(cell.stable_id.0, 0x741d_0857_640e_9fc4, "struct Cell");
    assert_eq!(cell.fields[0].stable_id.0, 0x53eb_ec4f_6958_03f3, "field Cell.x");
    assert_eq!(cell.fields[1].stable_id.0, 0x95c5_b49e_2fa7_3db9, "field Cell.y");

    let event = &contract.enums[0];
    assert_eq!(event.stable_id.0, 0xcfa3_af2f_5ed1_4df0, "enum Event");
    assert_eq!(event.variants[0].stable_id.0, 0xad45_75c2_a09e_68ca, "variant Started");
    assert_eq!(event.variants[1].stable_id.0, 0xb1b3_8d20_96ad_b205, "variant Ended");

    let log = &contract.host_functions[0];
    assert_eq!(log.stable_id.0, 0xfd1d_55d7_3e3d_4441, "host fn log");
    assert_eq!(
        log.parameters[0].stable_id.0,
        0x46d1_c221_b976_5897,
        "host fn log(message)"
    );

    let on_event = &contract.nexa_functions[0];
    assert_eq!(on_event.stable_id.0, 0xced5_39c2_2107_132d, "nexa fn on_event");
    assert_eq!(
        on_event.parameters[0].stable_id.0,
        0x3e42_b2a3_0f2a_4476,
        "nexa fn on_event(event)"
    );
}

#[test]
fn golden_descriptor_bytes_and_fingerprint_are_locked() {
    let contract = parse_contract(GOLDEN_SOURCE).expect("Golden source parses");
    let descriptor = abi_descriptor(&contract);
    assert_eq!(
        descriptor.bytes.as_slice(),
        GOLDEN_DESCRIPTOR_BYTES,
        "ABI descriptor bytes must remain byte-deterministic for equivalent semantics"
    );
    assert_eq!(
        descriptor.fingerprint.as_bytes(),
        &GOLDEN_FINGERPRINT,
        "Contract fingerprint must remain unchanged for equivalent semantics"
    );
}

#[test]
fn golden_generated_public_binding_api_snapshot_is_locked() {
    let contract = parse_contract(GOLDEN_SOURCE).expect("Golden source parses");
    let rust = generate_rust_for_source_file(&contract, "golden.contract.nexa")
        .expect("codegen succeeds");

    // Canonical public API snapshot from a structured syn::File walk (includes trait and
    // inherent-impl associated items and full signatures, not just `pub fn` prefixes).
    let public_items = public_api_snapshot(&rust);
    let expected: Vec<&str> = GOLDEN_PUBLIC_API_SNAPSHOT
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        public_items, expected,
        "generated public binding API drifted from the golden snapshot"
    );

    // Version metadata inside the binding stays at the frozen values.
    assert!(rust.contains("pub const CONTRACT_SYNTAX_VERSION: u16 = 3u16;"));
    assert!(rust.contains("pub const HOST_CONTRACT_SCHEMA_VERSION: u32 = 2;"));
    assert!(rust.contains("pub const ABI_DESCRIPTOR_VERSION: u16 = 2u16;"));

    // Codegen is byte-deterministic.
    assert_eq!(
        generate_rust_for_source_file(&contract, "golden.contract.nexa").expect("regenerate"),
        rust,
        "codegen must be deterministic"
    );
}

#[test]
fn invalid_source_file_name_fails_closed_and_cannot_inject_rust() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let contract = parse_contract(GOLDEN_SOURCE).expect("Golden source parses");

    // Missing basename (root path has no file name).
    let err = generate_rust_for_source_file(&contract, "/").unwrap_err();
    assert!(matches!(
        err,
        nexa_contract::CodegenError::InvalidSourceFileName { reason, .. }
            if reason.contains("no file name")
    ));

    // CR/LF in the basename must be rejected, not emitted into the header comment.
    let err = generate_rust_for_source_file(&contract, "evil\ncontract.contract.nexa").unwrap_err();
    assert!(matches!(
        err,
        nexa_contract::CodegenError::InvalidSourceFileName { reason, .. }
            if reason.contains("CR/LF")
    ));

    // Non-`.contract.nexa` suffix is advisory (allowed during the .nidl->.contract.nexa
    // migration); the file still generates with its basename provenance.
    let generated = generate_rust_for_source_file(&contract, "naughty.rs")
        .expect("advisory suffix does not fail generation");
    assert!(generated.starts_with("// Generated from naughty.rs. DO NOT EDIT.\n"));

    // Non-UTF-8 file name is rejected.
    let bad = std::path::PathBuf::from(OsStr::from_bytes(b"bad-\xff.contract.nexa"));
    let err = generate_rust_for_source_file(&contract, bad).unwrap_err();
    assert!(matches!(
        err,
        nexa_contract::CodegenError::InvalidSourceFileName { reason, .. }
            if reason.contains("UTF-8")
    ));
}

#[test]
fn golden_provenance_header_uses_the_contract_file_basename_only() {
    let contract = parse_contract(GOLDEN_SOURCE).expect("Golden source parses");

    let relative = generate_rust_for_source_file(&contract, "fixtures/golden.contract.nexa")
        .expect("path-aware codegen succeeds");
    assert!(
        relative.starts_with("// Generated from golden.contract.nexa. DO NOT EDIT.\n"),
        "provenance header must be stamped from the .contract.nexa file name"
    );

    let absolute = generate_rust_for_source_file(
        &contract,
        "/tmp/some/absolute/build/dir/golden.contract.nexa",
    )
    .expect("path-aware codegen succeeds");
    assert_eq!(
        relative, absolute,
        "directories and absolute path prefixes must never leak into generated output"
    );

    // The name-based variant keeps its own explicit provenance form.
    let by_name = generate_rust(&contract).expect("codegen succeeds");
    assert!(by_name.starts_with("// @generated from Contract `Golden`. DO NOT EDIT.\n"));
}

/// Canonicalizes the generated file's public API into a sorted, deduplicated signature list
/// using a structured `syn::File` walk (public top-level items, struct fields, enum variants,
/// and all public associated items of traits and inherent impls, with full signatures).
fn toks(t: &dyn quote::ToTokens) -> String {
    quote::ToTokens::to_token_stream(t).to_string()
}

fn public_api_snapshot(source: &str) -> Vec<String> {
    use syn::Visibility;
    let ast: syn::File = syn::parse_str(source).expect("generated binding parses as Rust");
    let mut out = Vec::new();
    for item in ast.items {
        match item {
            syn::Item::Const(c) if matches!(c.vis, Visibility::Public(_)) => {
                out.push(format!("pub const {}: {}", c.ident, toks(&c.ty)));
            }
            syn::Item::Static(s) if matches!(s.vis, Visibility::Public(_)) => {
                out.push(format!("pub static {}: {}", s.ident, toks(&s.ty)));
            }
            syn::Item::Type(t) if matches!(t.vis, Visibility::Public(_)) => {
                out.push(format!("pub type {}{} = {}", t.ident, toks(&t.generics), toks(&t.ty)));
            }
            syn::Item::Struct(st) => {
                if matches!(st.vis, Visibility::Public(_)) {
                    out.push(format!("pub struct {}{}", st.ident, toks(&st.generics)));
                }
                if let syn::Fields::Named(fields) = &st.fields {
                    for f in &fields.named {
                        if matches!(f.vis, Visibility::Public(_)) {
                            out.push(format!("pub {}: {}", f.ident.as_ref().unwrap(), toks(&f.ty)));
                        }
                    }
                }
            }
            syn::Item::Enum(en) => {
                if matches!(en.vis, Visibility::Public(_)) {
                    out.push(format!("pub enum {}{}", en.ident, toks(&en.generics)));
                }
                for v in en.variants {
                    out.push(format!("pub variant {}::{}", en.ident, v.ident));
                }
            }
            syn::Item::Trait(tr) if matches!(tr.vis, Visibility::Public(_)) => {
                out.push(format!("pub trait {}{}", tr.ident, toks(&tr.generics)));
                for ti in tr.items {
                    match ti {
                        syn::TraitItem::Const(c) => out.push(format!(
                            "pub trait {}::const {}: {}",
                            tr.ident, c.ident, toks(&c.ty)
                        )),
                        syn::TraitItem::Type(t) => {
                            out.push(format!("pub trait {}::type {}", tr.ident, t.ident));
                        }
                        syn::TraitItem::Fn(method) => out.push(format!(
                            "pub trait {}::{}{}",
                            tr.ident,
                            method.sig.ident,
                            method_sig_tail(&method.sig)
                        )),
                        _ => {}
                    }
                }
            }
            syn::Item::Impl(imp) if imp.trait_.is_none() => {
                let self_ty = toks(&*imp.self_ty);
                for ai in imp.items {
                    match ai {
                        syn::ImplItem::Const(c) if matches!(c.vis, Visibility::Public(_)) => {
                            out.push(format!(
                                "impl {}::const {}: {}",
                                self_ty, c.ident, toks(&c.ty)
                            ));
                        }
                        syn::ImplItem::Type(t) if matches!(t.vis, Visibility::Public(_)) => {
                            out.push(format!("impl {}::type {}", self_ty, t.ident));
                        }
                        syn::ImplItem::Fn(m) if matches!(m.vis, Visibility::Public(_)) => out.push(
                            format!("impl {}::{}{}", self_ty, m.sig.ident, method_sig_tail(&m.sig)),
                        ),
                        _ => {}
                    }
                }
            }
            syn::Item::Fn(f) if matches!(f.vis, Visibility::Public(_)) => {
                out.push(format!("pub fn {}", f.sig.ident));
            }
            _ => {}
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Renders the tail of a function signature (generics, parameters, where clause, return type),
/// excluding the body.
fn method_sig_tail(sig: &syn::Signature) -> String {
    let params: Vec<String> = sig
        .inputs
        .iter()
        .map(|p| match p {
            syn::FnArg::Typed(pt) => {
                let name = match &*pt.pat {
                    syn::Pat::Ident(id) => id.ident.to_string(),
                    syn::Pat::Wild(_) => "_".to_owned(),
                    _ => "?".to_owned(),
                };
                format!("({name}: {})", toks(&pt.ty))
            }
            syn::FnArg::Receiver(r) => {
                if r.reference.is_some() {
                    if r.mutability.is_some() {
                        "(&mut self)".into()
                    } else {
                        "(&self)".into()
                    }
                } else if r.mutability.is_some() {
                    "(mut self)".into()
                } else {
                    "(self)".into()
                }
            }
        })
        .collect();
    let where_s = if sig.generics.where_clause.is_some() {
        " where ..."
    } else {
        ""
    };
    let ret = match &sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => format!(" -> {}", toks(&**ty)),
    };
    format!("{}{}{}{}", toks(&sig.generics), params.join(""), where_s, ret)
}

const GOLDEN_DESCRIPTOR_BYTES: &[u8] = &[
    0x18, 0x00, 0x00, 0x00, 0x6e, 0x65, 0x78, 0x61, 0x2e, 0x63, 0x6f, 0x6e, 0x74, 0x72, 0x61,
    0x63, 0x74, 0x2d, 0x64, 0x65, 0x73, 0x63, 0x72, 0x69, 0x70, 0x74, 0x6f, 0x72, 0x02, 0x00,
    0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0x66, 0x75, 0x6c, 0x6c, 0x2d, 0x63, 0x6f, 0x6e, 0x74,
    0x72, 0x61, 0x63, 0x74, 0xb0, 0xdf, 0x77, 0x24, 0xe7, 0x48, 0x9f, 0x07, 0x06, 0x00, 0x00,
    0x00, 0x47, 0x6f, 0x6c, 0x64, 0x65, 0x6e, 0x03, 0x00, 0x00, 0x00, 0xed, 0x3a, 0x94, 0xc5,
    0xe2, 0xed, 0x89, 0x40, 0xc6, 0xf8, 0xcc, 0x2c, 0xa6, 0x82, 0x8b, 0x24, 0x1c, 0x1e, 0x63,
    0xc8, 0x7c, 0x36, 0x61, 0xee, 0xf9, 0xfa, 0xf8, 0xc1, 0x12, 0x49, 0x7c, 0xdf, 0x7f, 0x20,
    0xf1, 0xe0, 0xf0, 0x1d, 0x59, 0x54, 0xcc, 0x0a, 0x7e, 0x32, 0x8d, 0x31, 0x06, 0xfd, 0x2c,
    0x5a, 0xd7, 0xeb, 0xa2, 0x3f, 0x5d, 0x67, 0x95, 0x32, 0xfd, 0x2c, 0x71, 0x5d, 0x50, 0x74,
    0x30, 0x01, 0xa0, 0x34, 0x8b, 0x60, 0xf3, 0x99, 0xad, 0x7d, 0xb8, 0xd3, 0xb2, 0xb0, 0xe6,
    0x9e, 0xde, 0x07, 0x93, 0x43, 0x51, 0x7c, 0xc0, 0x8c, 0x69, 0xdb, 0x97, 0x83, 0xbe, 0x6d,
    0xfd, 0x9d, 0x01, 0x00, 0x00, 0x00, 0x7d, 0xc3, 0x12, 0x83, 0x84, 0xf6, 0x00, 0x77, 0x9c,
    0xc4, 0xd8, 0xe1, 0x67, 0x65, 0x1f, 0xb6, 0x9a, 0x34, 0x24, 0xc4, 0x52, 0x91, 0x4c, 0xcc,
    0x5f, 0xa0, 0xd5, 0x3b, 0x2e, 0xd3, 0xf4, 0xa9, 0x01, 0x00, 0x00, 0x00, 0x1c, 0x23, 0xda,
    0x73, 0xa1, 0xaf, 0x10, 0x87, 0x5f, 0xc6, 0x95, 0x25, 0x8c, 0xc2, 0x03, 0x3c, 0x8a, 0x4a,
    0xa6, 0x4b, 0x3d, 0x53, 0x58, 0x79, 0x5c, 0x7a, 0x3f, 0xd8, 0x25, 0xc2, 0x2f, 0xf4,
];

const GOLDEN_FINGERPRINT: [u8; 32] = [
    0x35, 0xc9, 0xe9, 0xe3, 0x88, 0xd3, 0x06, 0x8e, 0xc2, 0xed, 0xcf, 0x60, 0x7f, 0xf5, 0x4e,
    0xc4, 0xa6, 0x41, 0x49, 0xa6, 0xa2, 0x64, 0x47, 0xf5, 0xd0, 0x2c, 0xce, 0x87, 0x9d, 0x78,
    0xa4, 0x6a,
];

const GOLDEN_PUBLIC_API_SNAPSHOT: &str = r"impl CellRef < 'a >::x(self) -> Result < i32 , nexa_runtime :: HostTrap >
impl CellRef < 'a >::y(self) -> Result < i32 , nexa_runtime :: HostTrap >
impl Event::nexa_tag(&self) -> u32
impl GeneratedHostRegistry < H >::new(host: H) -> Self
impl OnEvent::const NAME: & 'static str
impl OnEvent::const STABLE_ID: nexa_runtime :: StableId
impl __NexaArrayRef < 'a , T >::get(self)(index: usize) -> :: std :: result :: Result < T , nexa_runtime :: HostTrap >
impl __NexaArrayRef < 'a , T >::is_empty(self) -> bool
impl __NexaArrayRef < 'a , T >::iter(self) -> impl :: std :: iter :: ExactSizeIterator < Item = :: std :: result :: Result < T , nexa_runtime :: HostTrap > , > + 'a
impl __NexaArrayRef < 'a , T >::len(self) -> usize
impl __NexaBufferRef < 'a , T >::get(self)(index: usize) -> :: std :: result :: Result < T , nexa_runtime :: HostTrap >
impl __NexaBufferRef < 'a , T >::is_empty(self) -> bool
impl __NexaBufferRef < 'a , T >::iter(self) -> impl :: std :: iter :: ExactSizeIterator < Item = :: std :: result :: Result < T , nexa_runtime :: HostTrap > , > + 'a
impl __NexaBufferRef < 'a , T >::len(self) -> usize
pub const ABI_DESCRIPTOR_VERSION: u16
pub const CONTRACT_DESCRIPTOR: & [u8]
pub const CONTRACT_FINGERPRINT: [u8 ; 32]
pub const CONTRACT_RUNTIME_ID: nexa_runtime :: StableId
pub const CONTRACT_SOURCE_NAME: & str
pub const CONTRACT_SYNTAX_VERSION: u16
pub const HOST_CONTRACT_SCHEMA_VERSION: u32
pub const SOURCE: & str
pub enum Event
pub enum EventRef< 'a >
pub enum OnEvent
pub event: Event
pub fn contract
pub fn registry
pub host: H
pub static HOST_FUNCTION_AUTHORITIES: :: std :: sync :: LazyLock < [nexa_runtime :: HostFunctionAuthority ; 1usize] , >
pub struct Cell
pub struct CellRef< 'a >
pub struct Entity
pub struct GeneratedHostRegistry< H >
pub struct GeneratedHostStub
pub struct HostError
pub struct OnEventArgs
pub struct __NexaArrayRef< 'a , T >
pub struct __NexaBufferRef< 'a , T >
pub trait GoldenHost
pub trait GoldenHost::log< 'a >(&mut self)(context: & mut nexa_runtime :: ResourceContext < '_ >)(message: & 'a str) -> :: std :: result :: Result < () , HostError >
pub type OnEventOutput = :: std :: vec :: Vec < i32 >
pub variant Event::Ended
pub variant Event::Started
pub variant EventRef::Ended
pub variant EventRef::Started
pub variant EventRef::__Lifetime
pub x: i32
pub y: i32";

