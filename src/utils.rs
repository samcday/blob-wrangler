use std::io::{Error, ErrorKind, Write};
use std::{path::PathBuf, process::Command};

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
            Err(Error::new(ErrorKind::Other, err_str))
        }
    } else {
        let err_str = format!("{}: unable to execute command!", command);
        Err(Error::new(ErrorKind::Other, err_str))
    }
}

pub fn undivert(file: &PathBuf) -> Result<(), Error> {
    let orig = format!("{}", file.display());
    let args = vec!["--package", "droid-juicer", "--rename", "--remove", &orig];

    execute("/usr/bin/dpkg-divert", Some(args))
}
