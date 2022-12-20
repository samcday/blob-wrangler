use std::{ process::Command, path::PathBuf };
use std::io::{ Error, ErrorKind, Write };

pub fn execute(command: &str, arguments: Option<Vec<&str>>) -> Result<(), Error> {
    let mut exe = Command::new(&command);

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

fn execute_divert(file: &PathBuf, divert: bool) -> Result<(), Error> {
    let orig = format!("{}", file.display());
    let diverted = format!("{}.juicer", file.display());
    let mut args = vec!["--package", "droid-juicer", "--rename"];

    if divert {
        args.push("--add");
        args.push("--divert");
        args.push(&diverted);
    } else {
        args.push("--remove");
    }
    args.push(&orig);

    execute("/usr/bin/dpkg-divert", Some(args))
}

pub fn divert(file: &PathBuf) -> Result<(), Error> {
    execute_divert(file, true)
}

pub fn undivert(file: &PathBuf) -> Result<(), Error> {
    execute_divert(file, false)
}
