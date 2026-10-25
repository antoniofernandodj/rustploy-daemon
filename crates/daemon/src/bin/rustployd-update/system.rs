//! Tudo que o updater pede ao sistema (rodar comando, esperar) passa por aqui, para
//! os testes trocarem o `dpkg` e o `systemctl` por um dublê.

use std::{io, process::Command, process::Stdio, time::Duration};

#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait System {
    /// Roda `program` com `args`, num ambiente limpo (só `PATH`, `LANG` e `envs`).
    /// Sem shell: cada argumento chega ao programa exatamente como está.
    fn run(&self, program: &str, args: &[&str], envs: &[(&str, &str)])
    -> io::Result<CommandOutput>;

    fn sleep(&self, duration: Duration);
}

pub struct RealSystem;

impl System for RealSystem {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> io::Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LANG", "C")
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            .output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
