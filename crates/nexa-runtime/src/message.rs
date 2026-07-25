use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticCode(&'static str);

impl DiagnosticCode {
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self(code)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineMessage {
    bytes: [u8; 96],
    len: u8,
}

impl InlineMessage {
    #[must_use]
    pub fn new(message: &str) -> Self {
        let mut len = message.len().min(96);
        while !message.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0; 96];
        bytes[..len].copy_from_slice(&message.as_bytes()[..len]);
        Self {
            bytes,
            len: u8::try_from(len).expect("inline message capacity fits u8"),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("InlineMessage only copies complete UTF-8 code points")
    }
}

impl fmt::Display for InlineMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMessage {
    Static(&'static str),
    Code { code: DiagnosticCode, argument: u64 },
    Inline(InlineMessage),
}

impl fmt::Debug for RuntimeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(message) => message.fmt(formatter),
            Self::Inline(message) => message.as_str().fmt(formatter),
            Self::Code { code, argument } => formatter
                .debug_struct("Code")
                .field("code", code)
                .field("argument", argument)
                .finish(),
        }
    }
}

impl RuntimeMessage {
    #[must_use]
    pub fn inline(message: &str) -> Self {
        Self::Inline(InlineMessage::new(message))
    }
}

impl From<&'static str> for RuntimeMessage {
    fn from(message: &'static str) -> Self {
        Self::Static(message)
    }
}

impl fmt::Display for RuntimeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(message) => formatter.write_str(message),
            Self::Code { code, argument } => write!(formatter, "{code} ({argument})"),
            Self::Inline(message) => message.fmt(formatter),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{DiagnosticCode, InlineMessage, RuntimeMessage};

    #[test]
    fn inline_message_has_fixed_storage_and_preserves_utf8_boundaries() {
        let source = "界".repeat(33);
        let message = InlineMessage::new(&source);

        assert_eq!(size_of::<InlineMessage>(), 97);
        assert_eq!(message.as_str(), "界".repeat(32));
        assert_eq!(message.as_str().len(), 96);
    }

    #[test]
    fn runtime_message_renders_without_owned_storage() {
        let code = RuntimeMessage::Code {
            code: DiagnosticCode::new("NX5001"),
            argument: 7,
        };
        let inline = RuntimeMessage::inline("host rejected request");

        assert_eq!(RuntimeMessage::Static("trap").to_string(), "trap");
        assert_eq!(code.to_string(), "NX5001 (7)");
        assert_eq!(inline.to_string(), "host rejected request");
    }
}
