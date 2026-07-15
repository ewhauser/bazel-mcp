use bazel_mcp_types::BazelCommand;

use crate::{PolicyConfig, PolicyError};

pub fn validate_command(config: &PolicyConfig, command: &BazelCommand) -> Result<(), PolicyError> {
    if config.denied_commands.contains(command.as_str()) {
        return Err(PolicyError::CommandDenied(command.to_string()));
    }
    if !config.allowed_commands.contains(command.as_str()) {
        return Err(PolicyError::CommandUnsupported(command.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_commands_are_denied_by_default() {
        let config = PolicyConfig::default();
        for command in [
            BazelCommand::Clean,
            BazelCommand::MobileInstall,
            BazelCommand::Run,
            BazelCommand::Shutdown,
        ] {
            assert!(matches!(
                validate_command(&config, &command),
                Err(PolicyError::CommandDenied(_))
            ));
        }
    }
}
