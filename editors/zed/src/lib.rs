use zed_extension_api::{self as zed, LanguageServerId, Worktree};

struct NexaExtension;

impl zed::Extension for NexaExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<zed::Command> {
        let command = worktree.which("nexa").ok_or_else(|| {
            "the `nexa` executable is not on PATH; build or install nexa-cli first".to_owned()
        })?;
        Ok(zed::Command {
            command,
            args: vec!["lsp".to_owned()],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(NexaExtension);
