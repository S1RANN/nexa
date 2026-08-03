//! M5 stage-F `ExecutableModule` v1 (F1 slice): the formal load path
//! `Verified Bytecode -> ExecutableModuleBuilder -> ExecutableModule`.
//!
//! Each row predecodes the per-instruction static data the hot loop
//! recomputes today: the load-time attempt fuel (via the single source of
//! truth `static_instruction_fuel`, so rows and the portable interpreter
//! cannot diverge), the folded host-import surcharge, and the safepoint
//! flag. Operand-dependent surcharges stay dynamic and are marked as such.
//!
//! Portable bytecode remains the cache and safety boundary: nothing here is
//! ever serialized. Later F slices add dense identity resolution, hot/cold
//! metadata separation, and the interpreter switch-over.

use nexa_bytecode::Instruction;
use nexa_verifier::{ResolvedNominalOperand, VerifiedModule};

use crate::interpreter::{OpcodeCostTable, static_instruction_fuel};

/// One predecoded instruction row: the hot fields fixed at build time.
#[derive(Clone, Copy, Debug)]
pub struct ExecutableInstruction {
    pub instruction: Instruction,
    /// Verifier-proven dense nominal operand for field instructions.
    pub resolved_nominal: ResolvedNominalOperand,
    /// Full load-time attempt charge (base cost, static work, and the
    /// host-import surcharge for `HostCall`); `None` when the instruction
    /// carries an operand-dependent dynamic surcharge.
    pub static_fuel: Option<u64>,
    /// Fixed at build time; never recomputed per instruction.
    pub safepoint: bool,
}

#[derive(Clone, Debug)]
pub struct ExecutableFunction {
    rows: Vec<ExecutableInstruction>,
}

impl ExecutableFunction {
    #[must_use]
    pub fn rows(&self) -> &[ExecutableInstruction] {
        &self.rows
    }
}

/// Build-time self-validation failures: the module never becomes
/// executable when any row is inconsistent with the verified bytecode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutableBuildError {
    CostTableVersionMismatch { supported: u32, table: u32 },
    MissingHostImport { function: u32, pc: u32, import: u32 },
    JumpOutOfFunction { function: u32, pc: u32, target: u32 },
    RootMapOutOfFunction { function: u32, pc: u32 },
    FuelOverflow { function: u32, pc: u32 },
}

impl std::fmt::Display for ExecutableBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CostTableVersionMismatch { supported, table } => write!(
                formatter,
                "cost table version {table} is not the supported version {supported}"
            ),
            Self::MissingHostImport {
                function,
                pc,
                import,
            } => write!(
                formatter,
                "function {function} pc {pc} calls unresolved host import {import}"
            ),
            Self::JumpOutOfFunction {
                function,
                pc,
                target,
            } => write!(
                formatter,
                "function {function} pc {pc} jumps outside the function to {target}"
            ),
            Self::RootMapOutOfFunction { function, pc } => write!(
                formatter,
                "function {function} carries a root map for out-of-range pc {pc}"
            ),
            Self::FuelOverflow { function, pc } => write!(
                formatter,
                "function {function} pc {pc} overflows the static fuel charge"
            ),
        }
    }
}

impl std::error::Error for ExecutableBuildError {}

/// Predecoded execution form of one verified module (F1 slice).
#[derive(Clone, Debug)]
pub struct ExecutableModule {
    functions: Vec<ExecutableFunction>,
    cost_table_version: u32,
}

impl ExecutableModule {
    /// Builds and self-validates the predecoded rows for every function.
    ///
    /// Validation covers what F1 materializes: host-import resolvability
    /// (the folded surcharge must exist), jump-target legality, root-map pc
    /// mapping, and static fuel overflow. Full operand range legality
    /// remains the verifier's admission contract on the input.
    pub fn build(
        module: &VerifiedModule,
        costs: &OpcodeCostTable,
    ) -> Result<Self, ExecutableBuildError> {
        if costs.version != nexa_core::OPCODE_COST_TABLE_VERSION {
            return Err(ExecutableBuildError::CostTableVersionMismatch {
                supported: nexa_core::OPCODE_COST_TABLE_VERSION,
                table: costs.version,
            });
        }
        let nominal_shape = module.nominal_index_shape();
        let bytecode = module.module();
        let mut functions = Vec::with_capacity(bytecode.functions.len());
        for (function_index, function) in bytecode.functions.iter().enumerate() {
            let function_index = u32::try_from(function_index).unwrap_or(u32::MAX);
            let code_len = u32::try_from(function.code.len()).unwrap_or(u32::MAX);
            for root_map in &function.root_maps {
                if root_map.pc >= code_len {
                    return Err(ExecutableBuildError::RootMapOutOfFunction {
                        function: function_index,
                        pc: root_map.pc,
                    });
                }
            }
            let mut rows = Vec::with_capacity(function.code.len());
            for (pc, instruction) in function.code.iter().copied().enumerate() {
                let pc = u32::try_from(pc).unwrap_or(u32::MAX);
                match instruction {
                    Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. }
                        if target > code_len =>
                    {
                        return Err(ExecutableBuildError::JumpOutOfFunction {
                            function: function_index,
                            pc,
                            target,
                        });
                    }
                    _ => {}
                }
                let Ok(static_fuel) =
                    static_instruction_fuel(bytecode, nominal_shape, instruction, costs)
                else {
                    return Err(ExecutableBuildError::FuelOverflow {
                        function: function_index,
                        pc,
                    });
                };
                // The interpreter charges the host-import surcharge on top
                // of the attempt fuel; the row folds it at build time.
                let static_fuel = if let Instruction::HostCall { import, .. } = instruction {
                    let host_import = bytecode.host_imports.get(import as usize).ok_or(
                        ExecutableBuildError::MissingHostImport {
                            function: function_index,
                            pc,
                            import,
                        },
                    )?;
                    match static_fuel {
                        Some(fuel) => {
                            Some(fuel.checked_add(u64::from(host_import.fuel_cost)).ok_or(
                                ExecutableBuildError::FuelOverflow {
                                    function: function_index,
                                    pc,
                                },
                            )?)
                        }
                        None => None,
                    }
                } else {
                    static_fuel
                };
                rows.push(ExecutableInstruction {
                    instruction,
                    resolved_nominal: module.resolved_operand(function_index as usize, pc as usize),
                    static_fuel,
                    safepoint: crate::interpreter::is_safepoint(instruction, pc),
                });
            }
            functions.push(ExecutableFunction { rows });
        }
        Ok(Self {
            functions,
            cost_table_version: costs.version,
        })
    }

    #[must_use]
    pub fn functions(&self) -> &[ExecutableFunction] {
        &self.functions
    }

    #[must_use]
    pub const fn cost_table_version(&self) -> u32 {
        self.cost_table_version
    }

    /// Share of instruction rows whose whole charge is settled at load
    /// time (F1 coverage metric for the stage-F profile).
    #[must_use]
    pub fn static_fuel_coverage(&self) -> (usize, usize) {
        let mut static_rows = 0;
        let mut total = 0;
        for function in &self.functions {
            total += function.rows.len();
            static_rows += function
                .rows
                .iter()
                .filter(|row| row.static_fuel.is_some())
                .count();
        }
        (static_rows, total)
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder,
        Signature, ValueType,
    };
    use nexa_core::StableId;
    use nexa_verifier::{VerifierLimits, verify};

    use super::{ExecutableBuildError, ExecutableModule};
    use crate::interpreter::{OpcodeCostTable, is_safepoint};

    const CORPUS: &str = r#"
struct Pair { first: i32, second: i32, }
class Counter { mut value: i32, }
enum Signal { Quiet, Loud(i32), }

fn mixed(x: i32) -> i32 {
    let text: string = "predecode";
    let cell: Pair = Pair { first: x, second: text.byte_len() };
    let values: Array<i32> = Array::new();
    values.push(cell.first);
    let table: Map<i32, i32> = Map::new();
    table.set(1, cell.second);
    let signal: Signal = Signal::Loud(x);
    let selected: i32 = match signal {
        Signal::Quiet => 0,
        Signal::Loud(value) => value,
    };
    return helper(selected) + values.len() + table.len();
}
fn helper(x: i32) -> i32 {
    return x + 1;
}
fn update_counter() -> i32 {
    let counter: Counter = new Counter { value: 1 };
    counter.value = counter.value + 1;
    return counter.value;
}
"#;

    /// The operand-dependent surface frozen by F1: a row may be dynamic
    /// only for these instructions. Static misclassification of any of
    /// them would silently change charged fuel, so the set is pinned.
    fn dynamic_surface(instruction: Instruction) -> bool {
        matches!(
            instruction,
            Instruction::StandardIntrinsic { .. }
                | Instruction::StringLen { .. }
                | Instruction::StringRuneAt { .. }
                | Instruction::StringEqual { .. }
                | Instruction::StringConcat { .. }
                | Instruction::StructNew { .. }
                | Instruction::StructWith { .. }
                | Instruction::EnumEqual { .. }
                | Instruction::StructEqual { .. }
                | Instruction::ArrayPush { .. }
                | Instruction::ArrayInsert { .. }
                | Instruction::ArrayPop { .. }
                | Instruction::ArrayRemove { .. }
                | Instruction::ArrayClear { .. }
                | Instruction::MapGet { .. }
                | Instruction::MapRemove { .. }
                | Instruction::MapContains { .. }
                | Instruction::MapSet { .. }
                | Instruction::MapClear { .. }
                | Instruction::BufferSlice { .. }
                | Instruction::BufferCopy { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnVoid
                | Instruction::CleanupReturn
        )
    }

    #[test]
    fn rows_cover_the_corpus_and_pin_the_dynamic_surface() {
        let module = nexa_compiler::compile(CORPUS).expect("F1 corpus compiles");
        let costs = OpcodeCostTable::default();
        let executable = ExecutableModule::build(&module, &costs).expect("build executable");
        assert_eq!(
            executable.functions().len(),
            module.module().functions.len()
        );
        for (function, rows) in module.module().functions.iter().zip(executable.functions()) {
            assert_eq!(function.code.len(), rows.rows().len());
            for (pc, row) in rows.rows().iter().enumerate() {
                let pc = u32::try_from(pc).expect("test corpus pcs fit u32");
                assert_eq!(
                    row.safepoint,
                    is_safepoint(row.instruction, pc),
                    "safepoint flag diverges at pc {pc}"
                );
                if row.static_fuel.is_none() {
                    assert!(
                        dynamic_surface(row.instruction),
                        "instruction unexpectedly dynamic at pc {pc}: {:?}",
                        row.instruction
                    );
                } else {
                    assert!(
                        !dynamic_surface(row.instruction),
                        "operand-dependent instruction misclassified static at pc {pc}: {:?}",
                        row.instruction
                    );
                    assert!(row.static_fuel.unwrap_or(0) > 0, "static rows charge fuel");
                }
            }
        }
        let (static_rows, total) = executable.static_fuel_coverage();
        assert!(total > 0);
        assert!(
            static_rows * 2 > total,
            "most corpus rows settle at load time ({static_rows}/{total})"
        );
        assert!(
            static_rows < total,
            "the corpus keeps a dynamic remainder ({static_rows}/{total})"
        );
        for function in executable.functions() {
            for row in function.rows() {
                match row.instruction {
                    Instruction::StructGet { .. } | Instruction::StructWith { .. } => assert!(
                        matches!(
                            row.resolved_nominal,
                            nexa_verifier::ResolvedNominalOperand::StructField { .. }
                        ),
                        "struct field rows carry a verifier-proven dense index"
                    ),
                    Instruction::ClassGet { .. } | Instruction::ClassSet { .. } => assert!(
                        matches!(
                            row.resolved_nominal,
                            nexa_verifier::ResolvedNominalOperand::ClassField { .. }
                        ),
                        "class field rows carry a verifier-proven dense index and type"
                    ),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn host_call_rows_fold_the_import_surcharge() {
        let host = StableId::from_name("executable-f1-host");
        let import = HostImport {
            stable_id: StableId::from_name("executable-f1-host.effect"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: vec![],
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 37,
            async_result: None,
        };
        let mut builder = ModuleBuilder::new();
        builder.metadata(host, nexa_bytecode::StateSchema::default().fingerprint());
        let import_index = builder.host_import(import);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![],
                result: Some(ValueType::I32),
            },
            1,
        );
        function.effect(FunctionEffect::Task);
        function
            .emit(Instruction::HostCall {
                import: import_index,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        builder.function(function.finish().expect("host call function"));
        let module = verify(builder.finish(), VerifierLimits::default()).expect("verify module");
        let costs = OpcodeCostTable::default();
        let executable = ExecutableModule::build(&module, &costs).expect("build executable");
        let row = executable.functions()[0].rows()[0];
        let bare = costs.cost(row.instruction);
        assert_eq!(
            row.static_fuel,
            Some(bare + 37),
            "HostCall rows carry the folded import surcharge"
        );
    }

    #[test]
    fn build_rejects_a_foreign_cost_table_version() {
        let module = nexa_compiler::compile("fn value() -> i32 { return 7; }")
            .expect("version corpus compiles");
        let mut costs = OpcodeCostTable::default();
        costs.version += 1;
        assert!(matches!(
            ExecutableModule::build(&module, &costs),
            Err(ExecutableBuildError::CostTableVersionMismatch { .. })
        ));
    }
}
