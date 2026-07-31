//! Source-aware diagnostics shared by Nexa's frontend, analysis, tooling, and host APIs.
//!
//! Diagnostics refer to immutable source identities. Source text is retained once in a shared
//! [`SourceSnapshotRegistry`], so a batch can contain many cross-file labels without cloning the
//! source for every diagnostic.

mod batch;
mod code;
mod diagnostic;
mod render;
mod source;

pub use batch::{DiagnosticBatch, DiagnosticBatchLimits, DroppedCounts};
pub use code::{ERROR_CODE_TABLE, ErrorCode, ErrorCodeDefinition};
pub type DiagnosticCode = ErrorCode;
pub use diagnostic::{
    Diagnostic, Label, LabelStyle, RelatedLocation, Severity, TextEditSuggestion,
};
pub use render::{
    DiagnosticRenderer, HUMAN_POSITION_ENCODING, MACHINE_POSITION_ENCODING, RENDER_SCHEMA_VERSION,
    RenderError,
};
pub use source::{
    ByteRange, HumanPosition, HumanRange, SourceIdentity, SourceSnapshot, SourceSnapshotRegistry,
    SourceSnapshotRegistryBuilder, SourceSnapshotRegistryError, Utf16Position, Utf16Range,
};
