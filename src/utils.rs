use std::io::{Error, Write};
use std::process::Command;

pub fn execute(command: &str, arguments: Option<Vec<&str>>) -> Result<(), Error> {
    let mut exe = Command::new(command);

    if let Some(args) = arguments {
        exe.args(args);
    }

    let res = exe.output();
    if let Ok(out) = res {
        std::io::stdout().write_all(&out.stdout).unwrap();
        if out.status.success() {
            Ok(())
        } else {
            std::io::stderr().write_all(&out.stderr).unwrap();
            let err_str = format!("{} returned with exit code {}", command, out.status);
            Err(Error::other(err_str))
        }
    } else {
        let err_str = format!("{command}: unable to execute command!");
        Err(Error::other(err_str))
    }
}
