use std::{io, io::Write, process::Command, path::PathBuf};

pub fn execute(command: &str, arguments: Option<Vec<&str>>) {
    let mut exe = Command::new(&command);

    if let Some(args) = arguments {
        exe.args(args);
    }

    let res = exe.output();
    if let Ok(out) = res {
        println!("{}: command status: {}", command, out.status);
        io::stdout().write_all(&out.stdout).unwrap();
        io::stderr().write_all(&out.stderr).unwrap();
    } else {
        println!("{}: unable to execute command!", command);
    }
}

pub fn divert(file: &PathBuf) {
    let orig = format!("{}", file.display());
    let diverted = format!("{}.juicer", file.display());
    execute("/usr/bin/dpkg-divert",
            Some(vec!["--add",
                      "--rename",
                      "--package", "droid-juicer",
                      "--divert", diverted.as_str(),
                      orig.as_str()]));
}

pub fn undivert(file: &PathBuf) {
    let orig = format!("{}", file.display());
    execute("/usr/bin/dpkg-divert",
            Some(vec!["--remove",
                      "--rename",
                      "--package", "droid-juicer",
                      orig.as_str()]));
}
