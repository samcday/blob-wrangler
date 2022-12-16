use std::{io, io::Write, process::Command};

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
