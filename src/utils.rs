use std::io::{Error, Write};
use std::process::Command;

pub fn execute(command: &str, arguments: Option<Vec<&str>>) -> Result<(), Error> {
    let mut exe = Command::new(command);

    if let Some(args) = arguments {
        exe.args(args);
    }

    let out = exe.output().map_err(|error| {
        Error::new(
            error.kind(),
            format!("{command}: unable to execute command: {error}"),
        )
    })?;
    std::io::stdout().write_all(&out.stdout)?;
    if out.status.success() {
        Ok(())
    } else {
        std::io::stderr().write_all(&out.stderr)?;
        let err_str = format!("{} returned with exit code {}", command, out.status);
        Err(Error::other(err_str))
    }
}
