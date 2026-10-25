//! A instalação de um pacote assinado, em duas fases:
//!
//! 1. [`Updater::prepare`] — tudo que **recusa** sem mexer no sistema: trava, cópia segura,
//!    assinatura, nome/versão/arquitetura do pacote, sem downgrade.
//! 2. [`Updater::execute`] — o que **muda** o sistema: backup dos binários, `dpkg -i`,
//!    espera o serviço ficar estável e, se não ficar, restaura os binários.
//!
//! A separação existe porque o `dpkg` reinicia o daemon, que é quem pediu: a resposta
//! "aceito" tem de sair entre as duas fases. Ver `main.rs`.

use crate::system::{CommandOutput, System};
use std::{
    fs, io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use update_core::{
    outcome::{OutcomeKind, UpdateOutcome, write_outcome},
    package::{Expectations, PackageFacts, check_package, parse_dpkg_fields},
    signature, staging,
};

/// Onde o updater lê e escreve.
pub struct Paths {
    /// Único diretório de onde ele aceita um `.deb` (o daemon escreve aqui).
    pub allowed_dir: PathBuf,
    /// Estado do próprio updater (só root escreve): cópias, backup, trava e resultado.
    pub state_dir: PathBuf,
    /// Binários que o pacote instala e que o backup/rollback cobrem.
    pub managed_binaries: Vec<PathBuf>,
}

impl Paths {
    fn staging_dir(&self) -> PathBuf {
        self.state_dir.join("staging")
    }
    fn backup_dir(&self) -> PathBuf {
        self.state_dir.join("backup")
    }
    pub fn outcome_file(&self) -> PathBuf {
        self.state_dir.join("last.json")
    }
    fn lock_file(&self) -> PathBuf {
        self.state_dir.join("lock")
    }
}

pub struct Settings {
    /// Nome fixo do pacote que este updater instala. Um `.deb` de outro pacote é recusado.
    pub package: String,
    /// Unidade systemd cuja estabilidade decide entre "instalado" e "voltar atrás".
    pub service: String,
    /// Chaves públicas minisign (ver `update_core::signature::TRUSTED_PUBLIC_KEYS`).
    pub trusted_keys: Vec<String>,
    pub max_deb_bytes: u64,
    /// Intervalo entre as checagens de `systemctl is-active`.
    pub poll_interval: Duration,
    /// Quantas checagens seguidas "active" valem como estável.
    pub stable_polls: u32,
    /// Teto de checagens antes de desistir e voltar atrás.
    pub max_polls: u32,
}

const MAX_SIGNATURE_BYTES: u64 = 8 * 1024;

pub struct Updater<'a, S: System> {
    pub paths: &'a Paths,
    pub settings: &'a Settings,
    pub system: &'a S,
}

/// Um rollback já aprovado. Segura a trava até o fim.
pub struct RollbackPlan {
    _lock: fs::File,
    pub previous_version: String,
}

/// Um pacote já aprovado. Segura a trava até o fim da instalação.
pub struct Prepared {
    _lock: fs::File,
    staged_deb: PathBuf,
    staged_signature: PathBuf,
    pub facts: PackageFacts,
}

impl<S: System> Updater<'_, S> {
    // ── fase 1: recusar sem mexer em nada ───────────────────────────────────────────

    pub fn prepare(&self, deb: &str, signature_path: &str) -> Result<Prepared, String> {
        fs::create_dir_all(&self.paths.state_dir)
            .map_err(|error| format!("estado do updater: {error}"))?;
        // A trava vem ANTES de qualquer escrita de resultado: quem perde a corrida não pode
        // apagar o `last.json` (ou o "em andamento") de quem está instalando.
        let lock = self.acquire_lock()?;

        let mut known_version = String::new();
        match self.verify_and_check(deb, signature_path, &mut known_version) {
            Ok((staged_deb, staged_signature, facts)) => Ok(Prepared {
                _lock: lock,
                staged_deb,
                staged_signature,
                facts,
            }),
            Err(message) => {
                self.record(&known_version, OutcomeKind::Failed, &message);
                Err(message)
            }
        }
    }

    fn acquire_lock(&self) -> Result<fs::File, String> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.paths.lock_file())
            .map_err(|error| format!("trava do updater: {error}"))?;
        file.try_lock()
            .map_err(|_| "outra atualização já está em andamento".to_string())?;
        Ok(file)
    }

    fn verify_and_check(
        &self,
        deb: &str,
        signature_path: &str,
        known_version: &mut String,
    ) -> Result<(PathBuf, PathBuf, PackageFacts), String> {
        // 1. Cópia para um diretório só do root: daqui em diante o daemon não troca mais nada.
        let staging_dir = self.paths.staging_dir();
        let staged_deb = staging::stage_file(
            Path::new(deb),
            &self.paths.allowed_dir,
            &staging_dir,
            self.settings.max_deb_bytes,
        )
        .map_err(|error| format!("pacote recusado: {error}"))?;
        let staged_signature = staging::stage_file(
            Path::new(signature_path),
            &self.paths.allowed_dir,
            &staging_dir,
            MAX_SIGNATURE_BYTES,
        )
        .map_err(|error| format!("assinatura recusada: {error}"))?;

        // 2. Assinatura ANTES de qualquer outra leitura: o `dpkg-deb` só toca num arquivo
        //    já provado legítimo.
        let deb_bytes =
            fs::read(&staged_deb).map_err(|error| format!("lendo o pacote: {error}"))?;
        let signature_text = fs::read_to_string(&staged_signature)
            .map_err(|error| format!("lendo a assinatura: {error}"))?;
        let trusted: Vec<&str> = self
            .settings
            .trusted_keys
            .iter()
            .map(String::as_str)
            .collect();
        let claims = signature::verify(&deb_bytes, &signature_text, &trusted)
            .map_err(|error| error.to_string())?;
        *known_version = claims.version.clone();
        drop(deb_bytes);

        // 3. O que o próprio pacote diz de si, a arquitetura da máquina e a versão instalada.
        let staged_deb_text = staged_deb.to_string_lossy().into_owned();
        let fields = self.run_checked(
            "dpkg-deb",
            &["-f", &staged_deb_text, "Package", "Version", "Architecture"],
            &[],
        )?;
        let facts = parse_dpkg_fields(&fields.stdout).map_err(|error| error.to_string())?;
        let machine_architecture = self
            .run_checked("dpkg", &["--print-architecture"], &[])?
            .stdout
            .trim()
            .to_string();
        let installed_version = self.installed_version()?;

        check_package(
            &claims,
            &facts,
            &Expectations {
                package: &self.settings.package,
                machine_architecture: &machine_architecture,
                installed_version: installed_version.as_deref(),
            },
        )
        .map_err(|error| error.to_string())?;

        Ok((staged_deb, staged_signature, facts))
    }

    fn installed_version(&self) -> Result<Option<String>, String> {
        let output = self
            .system
            .run(
                "dpkg-query",
                &["-W", "-f=${Version}", &self.settings.package],
                &[],
            )
            .map_err(|error| format!("dpkg-query: {error}"))?;
        let version = output.stdout.trim().to_string();
        // Pacote ainda não instalado: o dpkg-query sai com erro e sem saída.
        Ok((output.success && !version.is_empty()).then_some(version))
    }

    // ── fase 2: mudar o sistema ─────────────────────────────────────────────────────

    pub fn execute(&self, prepared: Prepared) -> UpdateOutcome {
        let version = prepared.facts.version.clone();
        self.record(&version, OutcomeKind::InProgress, "instalando");

        if let Err(error) = self.backup_binaries() {
            return self.finish(
                &prepared,
                &version,
                OutcomeKind::Failed,
                format!("backup dos binários: {error}"),
            );
        }

        let staged = prepared.staged_deb.to_string_lossy().into_owned();
        let installed = self.system.run(
            "dpkg",
            &["-i", "--force-confold", &staged],
            &[
                ("DEBIAN_FRONTEND", "noninteractive"),
                ("RUSTPLOY_UPDATING", "1"),
            ],
        );
        match installed {
            Ok(output) if output.success => {}
            other => {
                let detail = match other {
                    Ok(output) => tail(&output),
                    Err(error) => error.to_string(),
                };
                // O dpkg pode ter trocado os binários antes de falhar: devolve os antigos.
                let restored = self.restore_and_restart();
                let message = format!("o dpkg falhou: {detail}{}", restore_note(&restored));
                return self.finish(&prepared, &version, OutcomeKind::Failed, message);
            }
        }

        if self.wait_until_stable() {
            return self.finish(&prepared, &version, OutcomeKind::Installed, "instalado");
        }

        let restored = self.restore_and_restart();
        let message = format!(
            "o serviço {} não ficou estável depois da instalação; binários anteriores restaurados{}",
            self.settings.service,
            restore_note(&restored)
        );
        self.finish(&prepared, &version, OutcomeKind::RolledBack, message)
    }

    /// Fase 1 do rollback: só **recusa** (trava e backup existente), sem mexer em nada. Mesma
    /// separação do `install`: a resposta "aceito" tem de sair antes de `execute_rollback`, porque
    /// reiniciar o serviço mata o daemon, que é quem pediu.
    pub fn prepare_rollback(&self) -> Result<RollbackPlan, String> {
        fs::create_dir_all(&self.paths.state_dir)
            .map_err(|error| format!("estado do updater: {error}"))?;
        let lock = self.acquire_lock()?;
        let backup_dir = self.paths.backup_dir();
        let previous_version = fs::read_to_string(backup_dir.join("version"))
            .map(|text| text.trim().to_string())
            .unwrap_or_default();

        let has_backup = self
            .paths
            .managed_binaries
            .iter()
            .any(|binary| backup_file(&backup_dir, binary).is_some_and(|file| file.exists()));
        if !has_backup {
            return Err(
                "não há backup para restaurar (nenhuma atualização foi feita por aqui)".to_string(),
            );
        }
        Ok(RollbackPlan {
            _lock: lock,
            previous_version,
        })
    }

    /// Fase 2: volta os binários para os do último backup (o único caminho que aceita uma versão
    /// menor), reinicia o serviço e espera ele ficar estável.
    pub fn execute_rollback(&self, plan: RollbackPlan) -> UpdateOutcome {
        let previous_version = plan.previous_version.clone();
        self.record(&previous_version, OutcomeKind::InProgress, "revertendo");
        let restored = self.restore_and_restart();
        let (kind, message) = match restored {
            Err(error) => (
                OutcomeKind::Failed,
                format!("não consegui reverter: {error}"),
            ),
            Ok(()) if self.wait_until_stable() => (
                OutcomeKind::RolledBack,
                "binários anteriores restaurados a pedido".to_string(),
            ),
            Ok(()) => (
                OutcomeKind::Failed,
                format!(
                    "binários restaurados, mas o serviço {} não ficou estável",
                    self.settings.service
                ),
            ),
        };
        self.record(&previous_version, kind, &message);
        UpdateOutcome::new(&self.settings.package, &previous_version, kind, message)
    }

    /// As duas fases juntas (nos testes não há cliente esperando uma resposta no meio).
    #[cfg(test)]
    pub fn rollback(&self) -> Result<UpdateOutcome, String> {
        let plan = self.prepare_rollback()?;
        Ok(self.execute_rollback(plan))
    }

    fn finish(
        &self,
        prepared: &Prepared,
        version: &str,
        kind: OutcomeKind,
        message: impl Into<String>,
    ) -> UpdateOutcome {
        let message = message.into();
        // A cópia de trabalho não tem mais serventia; o backup é que fica.
        let _ = fs::remove_file(&prepared.staged_deb);
        let _ = fs::remove_file(&prepared.staged_signature);
        self.record(version, kind, &message);
        UpdateOutcome::new(&self.settings.package, version, kind, message)
    }

    /// Grava o resultado. Falhar em gravar não pode impedir o resto: só vai para o log.
    fn record(&self, version: &str, kind: OutcomeKind, message: &str) {
        let outcome = UpdateOutcome::new(&self.settings.package, version, kind, message);
        if let Err(error) = write_outcome(&self.paths.outcome_file(), &outcome) {
            eprintln!("rustployd-update: não consegui gravar o resultado: {error}");
        }
    }

    fn run_checked(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<CommandOutput, String> {
        let output = self
            .system
            .run(program, args, envs)
            .map_err(|error| format!("{program}: {error}"))?;
        if output.success {
            Ok(output)
        } else {
            Err(format!("{program} falhou: {}", tail(&output)))
        }
    }

    // ── backup, restauração e estabilidade ──────────────────────────────────────────

    fn backup_binaries(&self) -> Result<(), String> {
        let backup_dir = self.paths.backup_dir();
        fs::create_dir_all(&backup_dir).map_err(|error| error.to_string())?;
        fs::set_permissions(&backup_dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        // Backup antigo de um binário que hoje não existe não pode sobreviver como se fosse
        // o "estado anterior": começa do zero.
        for entry in fs::read_dir(&backup_dir)
            .map_err(|error| error.to_string())?
            .flatten()
        {
            let _ = fs::remove_file(entry.path());
        }
        let version = self.installed_version()?.unwrap_or_default();
        fs::write(backup_dir.join("version"), &version).map_err(|error| error.to_string())?;
        for binary in &self.paths.managed_binaries {
            if !binary.exists() {
                continue;
            }
            let Some(target) = backup_file(&backup_dir, binary) else {
                continue;
            };
            fs::copy(binary, &target).map_err(|error| format!("{}: {error}", binary.display()))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn restore_and_restart(&self) -> Result<(), String> {
        let backup_dir = self.paths.backup_dir();
        for binary in &self.paths.managed_binaries {
            let Some(saved) = backup_file(&backup_dir, binary).filter(|file| file.exists()) else {
                continue;
            };
            replace_atomically(&saved, binary)
                .map_err(|error| format!("{}: {error}", binary.display()))?;
        }
        self.run_checked("systemctl", &["restart", &self.settings.service], &[])
            .map(|_| ())
    }

    /// `true` se o serviço ficou `active` por `stable_polls` checagens seguidas. Um serviço que
    /// cai em laço (`Restart=on-failure`) aparece como `activating` entre as quedas e zera a conta.
    fn wait_until_stable(&self) -> bool {
        let mut consecutive = 0;
        for _ in 0..self.settings.max_polls {
            let active = self
                .system
                .run("systemctl", &["is-active", &self.settings.service], &[])
                .map(|output| output.success && output.stdout.trim() == "active")
                .unwrap_or(false);
            if active {
                consecutive += 1;
                if consecutive >= self.settings.stable_polls {
                    return true;
                }
            } else {
                consecutive = 0;
            }
            self.system.sleep(self.settings.poll_interval);
        }
        false
    }
}

fn backup_file(backup_dir: &Path, binary: &Path) -> Option<PathBuf> {
    binary.file_name().map(|name| backup_dir.join(name))
}

/// Troca `target` por uma cópia de `source` sem nunca deixá-lo pela metade: copia para um nome
/// ao lado e renomeia. Funciona com o binário em execução (o processo velho segue com o inode antigo).
fn replace_atomically(source: &Path, target: &Path) -> io::Result<()> {
    let mut sibling = target.as_os_str().to_owned();
    sibling.push(".rustploy-restore");
    let sibling = PathBuf::from(sibling);
    fs::copy(source, &sibling)?;
    fs::set_permissions(&sibling, fs::Permissions::from_mode(0o755))?;
    fs::rename(&sibling, target)
}

fn restore_note(restored: &Result<(), String>) -> String {
    match restored {
        Ok(()) => String::new(),
        Err(error) => format!(" (e a restauração falhou: {error})"),
    }
}

/// Últimas linhas do que o comando disse, para a mensagem que a pessoa vai ler.
fn tail(output: &CommandOutput) -> String {
    let text = format!("{}\n{}", output.stderr.trim(), output.stdout.trim());
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let start = lines.len().saturating_sub(8);
    lines[start..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use update_core::outcome::read_outcome;

    const TEST_PUBLIC_KEY: &str = include_str!("../../../tests/fixtures/updater/test.pub");
    const PAYLOAD: &[u8] = include_bytes!("../../../tests/fixtures/updater/payload.bin");
    const SIGNATURE: &str = include_str!("../../../tests/fixtures/updater/payload.bin.minisig");

    type Script = Box<dyn Fn(&str, &[&str]) -> Option<CommandOutput>>;

    struct Fake {
        script: Script,
        calls: RefCell<Vec<String>>,
    }

    impl System for Fake {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            _envs: &[(&str, &str)],
        ) -> io::Result<CommandOutput> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            (self.script)(program, args)
                .ok_or_else(|| io::Error::other(format!("comando inesperado: {program} {args:?}")))
        }
        fn sleep(&self, _duration: Duration) {}
    }

    fn ok(stdout: &str) -> Option<CommandOutput> {
        Some(CommandOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn fail(stderr: &str) -> Option<CommandOutput> {
        Some(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        })
    }

    struct Fixture {
        _root: tempfile::TempDir,
        paths: Paths,
        settings: Settings,
        deb: String,
        signature: String,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let allowed_dir = root.path().join("updates");
        let state_dir = root.path().join("state");
        let bin_dir = root.path().join("bin");
        for directory in [&allowed_dir, &bin_dir] {
            fs::create_dir_all(directory).unwrap();
        }
        let daemon = bin_dir.join("rustployd");
        let helper = bin_dir.join("rustployd-fw");
        fs::write(&daemon, b"DAEMON-ANTIGO").unwrap();
        fs::write(&helper, b"FW-ANTIGO").unwrap();

        let deb = allowed_dir.join("rustployd_0.2.0-1_amd64.deb");
        let signature = allowed_dir.join("rustployd_0.2.0-1_amd64.deb.minisig");
        fs::write(&deb, PAYLOAD).unwrap();
        fs::write(&signature, SIGNATURE).unwrap();

        Fixture {
            deb: deb.to_string_lossy().into_owned(),
            signature: signature.to_string_lossy().into_owned(),
            paths: Paths {
                allowed_dir,
                state_dir,
                managed_binaries: vec![daemon, helper],
            },
            settings: Settings {
                package: "rustployd".into(),
                service: "rustployd".into(),
                trusted_keys: vec![TEST_PUBLIC_KEY.lines().nth(1).unwrap().trim().to_string()],
                max_deb_bytes: 1024 * 1024,
                poll_interval: Duration::ZERO,
                stable_polls: 3,
                max_polls: 10,
            },
            _root: root,
        }
    }

    impl Fixture {
        fn daemon(&self) -> String {
            fs::read_to_string(&self.paths.managed_binaries[0]).unwrap()
        }
        fn helper(&self) -> String {
            fs::read_to_string(&self.paths.managed_binaries[1]).unwrap()
        }
        fn outcome(&self) -> UpdateOutcome {
            read_outcome(&self.paths.outcome_file())
                .unwrap()
                .expect("sem last.json")
        }
    }

    /// Dublê do caso feliz: o `dpkg -i` troca os binários; o serviço fica `active`.
    fn happy(fixture: &Fixture, installed: &'static str) -> Fake {
        let binaries = fixture.paths.managed_binaries.clone();
        Fake {
            calls: RefCell::default(),
            script: Box::new(
                move |program, args| match (program, args.first().copied()) {
                    ("dpkg-deb", Some("-f")) => {
                        ok("Package: rustployd\nVersion: 0.2.0-1\nArchitecture: amd64\n")
                    }
                    ("dpkg", Some("--print-architecture")) => ok("amd64\n"),
                    ("dpkg-query", _) => ok(installed),
                    ("dpkg", Some("-i")) => {
                        fs::write(&binaries[0], b"DAEMON-NOVO").unwrap();
                        fs::write(&binaries[1], b"FW-NOVO").unwrap();
                        ok("")
                    }
                    ("systemctl", Some("is-active")) => ok("active\n"),
                    ("systemctl", Some("restart")) => ok(""),
                    _ => None,
                },
            ),
        }
    }

    fn updater<'a>(fixture: &'a Fixture, system: &'a Fake) -> Updater<'a, Fake> {
        Updater {
            paths: &fixture.paths,
            settings: &fixture.settings,
            system,
        }
    }

    fn called(system: &Fake, prefix: &str) -> bool {
        system
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with(prefix))
    }

    #[test]
    fn happy_path_installs_backs_up_and_records_success() {
        let fixture = fixture();
        let system = happy(&fixture, "0.1.9-1");
        let updater = updater(&fixture, &system);

        let prepared = updater.prepare(&fixture.deb, &fixture.signature).unwrap();
        assert_eq!(prepared.facts.version, "0.2.0-1");
        let outcome = updater.execute(prepared);

        assert_eq!(outcome.kind, OutcomeKind::Installed);
        assert_eq!(fixture.daemon(), "DAEMON-NOVO");
        assert_eq!(fixture.outcome().kind, OutcomeKind::Installed);
        assert_eq!(fixture.outcome().version, "0.2.0-1");
        // O backup guarda o binário ANTERIOR e a versão anterior.
        let backup = fixture.paths.state_dir.join("backup");
        assert_eq!(
            fs::read_to_string(backup.join("rustployd")).unwrap(),
            "DAEMON-ANTIGO"
        );
        assert_eq!(
            fs::read_to_string(backup.join("version")).unwrap(),
            "0.1.9-1"
        );
        // O dpkg recebe a CÓPIA do root, nunca o caminho do daemon, e sem perguntas.
        let calls = system.calls.borrow();
        let install = calls
            .iter()
            .find(|call| call.starts_with("dpkg -i"))
            .unwrap();
        assert!(install.contains("--force-confold"), "{install}");
        assert!(install.contains("/state/staging/"), "{install}");
        assert!(!install.contains("/updates/"), "{install}");
        // A cópia de trabalho some no fim.
        assert!(
            !fixture
                .paths
                .state_dir
                .join("staging/rustployd_0.2.0-1_amd64.deb")
                .exists()
        );
    }

    #[test]
    fn a_tampered_package_is_refused_before_anything_runs() {
        let fixture = fixture();
        fs::write(&fixture.deb, [PAYLOAD, b"extra"].concat()).unwrap();
        let system = happy(&fixture, "0.1.9-1");
        let updater = updater(&fixture, &system);

        let error = updater
            .prepare(&fixture.deb, &fixture.signature)
            .err()
            .unwrap();

        assert!(error.contains("assinatura inválida"), "{error}");
        // Nenhum comando rodou: o dpkg-deb nunca toca num arquivo não verificado.
        assert!(
            system.calls.borrow().is_empty(),
            "{:?}",
            system.calls.borrow()
        );
        assert_eq!(fixture.outcome().kind, OutcomeKind::Failed);
        assert_eq!(fixture.daemon(), "DAEMON-ANTIGO");
    }

    #[test]
    fn a_signature_from_an_untrusted_key_is_refused() {
        let mut fixture = fixture();
        fixture.settings.trusted_keys = vec![
            include_str!("../../../tests/fixtures/updater/other.pub")
                .lines()
                .nth(1)
                .unwrap()
                .trim()
                .to_string(),
        ];
        let system = happy(&fixture, "0.1.9-1");
        let error = updater(&fixture, &system)
            .prepare(&fixture.deb, &fixture.signature)
            .err()
            .unwrap();
        assert!(error.contains("assinatura inválida"), "{error}");
    }

    #[test]
    fn a_signed_package_of_another_product_is_refused() {
        let fixture = fixture();
        let mut system = happy(&fixture, "0.1.9-1");
        // O arquivo assinado diz ser o app desktop (a assinatura, de mentira, continua válida).
        system.script = Box::new(|program, args| match (program, args.first().copied()) {
            ("dpkg-deb", _) => ok("Package: rustploy-gui\nVersion: 0.2.0-1\nArchitecture: amd64\n"),
            ("dpkg", Some("--print-architecture")) => ok("amd64\n"),
            ("dpkg-query", _) => ok("0.1.9-1"),
            _ => None,
        });
        let error = updater(&fixture, &system)
            .prepare(&fixture.deb, &fixture.signature)
            .err()
            .unwrap();
        assert!(error.contains("pacote errado"), "{error}");
        assert!(!called(&system, "dpkg -i"));
    }

    #[test]
    fn a_downgrade_or_the_same_version_is_refused() {
        for installed in ["0.2.0-1", "0.3.0-1", "1:0.0.1-1"] {
            let fixture = fixture();
            let system = happy(&fixture, installed);
            let error = updater(&fixture, &system)
                .prepare(&fixture.deb, &fixture.signature)
                .err()
                .unwrap();
            assert!(error.contains("downgrade"), "{installed}: {error}");
            assert!(!called(&system, "dpkg -i"));
        }
    }

    #[test]
    fn a_file_outside_the_updates_dir_is_refused() {
        let fixture = fixture();
        let outside = fixture.paths.state_dir.parent().unwrap().join("fora.deb");
        fs::write(&outside, PAYLOAD).unwrap();
        let system = happy(&fixture, "0.1.9-1");
        let error = updater(&fixture, &system)
            .prepare(outside.to_str().unwrap(), &fixture.signature)
            .err()
            .unwrap();
        assert!(error.contains("pacote recusado"), "{error}");
    }

    #[test]
    fn a_failed_dpkg_restores_the_old_binaries() {
        let fixture = fixture();
        let binaries = fixture.paths.managed_binaries.clone();
        let mut system = happy(&fixture, "0.1.9-1");
        // O dpkg troca o binário e só depois falha (ex.: o postinst deu erro).
        system.script = Box::new(
            move |program, args| match (program, args.first().copied()) {
                ("dpkg-deb", _) => {
                    ok("Package: rustployd\nVersion: 0.2.0-1\nArchitecture: amd64\n")
                }
                ("dpkg", Some("--print-architecture")) => ok("amd64\n"),
                ("dpkg-query", _) => ok("0.1.9-1"),
                ("dpkg", Some("-i")) => {
                    fs::write(&binaries[0], b"DAEMON-PELA-METADE").unwrap();
                    fail(
                        "dpkg: erro ao processar rustployd (--install): dependências não satisfeitas",
                    )
                }
                ("systemctl", Some("restart")) => ok(""),
                _ => None,
            },
        );
        let updater = updater(&fixture, &system);

        let outcome = updater.execute(updater.prepare(&fixture.deb, &fixture.signature).unwrap());

        assert_eq!(outcome.kind, OutcomeKind::Failed);
        assert!(
            outcome.message.contains("dependências não satisfeitas"),
            "{}",
            outcome.message
        );
        assert_eq!(fixture.daemon(), "DAEMON-ANTIGO");
        assert!(called(&system, "systemctl restart rustployd"));
    }

    #[test]
    fn an_unstable_service_rolls_back_the_binaries() {
        let fixture = fixture();
        let binaries = fixture.paths.managed_binaries.clone();
        let mut system = happy(&fixture, "0.1.9-1");
        system.script = Box::new(
            move |program, args| match (program, args.first().copied()) {
                ("dpkg-deb", _) => {
                    ok("Package: rustployd\nVersion: 0.2.0-1\nArchitecture: amd64\n")
                }
                ("dpkg", Some("--print-architecture")) => ok("amd64\n"),
                ("dpkg-query", _) => ok("0.1.9-1"),
                ("dpkg", Some("-i")) => {
                    fs::write(&binaries[0], b"DAEMON-QUEBRADO").unwrap();
                    ok("")
                }
                // Cai em laço: nunca fica "active".
                ("systemctl", Some("is-active")) => Some(CommandOutput {
                    success: false,
                    stdout: "activating\n".into(),
                    stderr: String::new(),
                }),
                ("systemctl", Some("restart")) => ok(""),
                _ => None,
            },
        );
        let updater = updater(&fixture, &system);

        let outcome = updater.execute(updater.prepare(&fixture.deb, &fixture.signature).unwrap());

        assert_eq!(outcome.kind, OutcomeKind::RolledBack);
        assert!(
            outcome.message.contains("não ficou estável"),
            "{}",
            outcome.message
        );
        assert_eq!(fixture.daemon(), "DAEMON-ANTIGO");
        assert_eq!(fixture.helper(), "FW-ANTIGO");
        assert!(called(&system, "systemctl restart rustployd"));
        assert_eq!(fixture.outcome().kind, OutcomeKind::RolledBack);
    }

    #[test]
    fn a_flapping_service_resets_the_stability_count() {
        let fixture = fixture();
        let polls = RefCell::new(0u32);
        // active, active, QUEDA, active, active, active → estável só depois da queda.
        let pattern = [true, true, false, true, true, true];
        let system = Fake {
            calls: RefCell::default(),
            script: Box::new(move |_, _| {
                let mut polls = polls.borrow_mut();
                let active = pattern.get(*polls as usize).copied().unwrap_or(true);
                *polls += 1;
                if active { ok("active\n") } else { fail("") }
            }),
        };
        let updater = updater(&fixture, &system);
        assert!(updater.wait_until_stable());
        assert_eq!(
            system.calls.borrow().len(),
            6,
            "a queda zerou a conta e foram precisas 6 checagens"
        );
    }

    #[test]
    fn a_second_install_while_one_runs_is_refused_without_touching_the_result() {
        let fixture = fixture();
        let system = happy(&fixture, "0.1.9-1");
        let updater = updater(&fixture, &system);
        let first = updater.prepare(&fixture.deb, &fixture.signature).unwrap();
        updater.record("0.2.0-1", OutcomeKind::InProgress, "instalando");

        let error = updater
            .prepare(&fixture.deb, &fixture.signature)
            .err()
            .unwrap();

        assert!(error.contains("já está em andamento"), "{error}");
        // O "em andamento" de quem instala não foi sobrescrito por um "falhou".
        assert_eq!(fixture.outcome().kind, OutcomeKind::InProgress);
        drop(first);
        // Liberada a trava, uma nova tentativa volta a funcionar.
        assert!(updater.prepare(&fixture.deb, &fixture.signature).is_ok());
    }

    #[test]
    fn rollback_restores_the_backup_and_needs_one_to_exist() {
        let fixture = fixture();
        let system = happy(&fixture, "0.1.9-1");
        let updater = updater(&fixture, &system);

        let error = updater.rollback().unwrap_err();
        assert!(error.contains("não há backup"), "{error}");

        updater.execute(updater.prepare(&fixture.deb, &fixture.signature).unwrap());
        assert_eq!(fixture.daemon(), "DAEMON-NOVO");

        let outcome = updater.rollback().unwrap();
        assert_eq!(outcome.kind, OutcomeKind::RolledBack);
        assert_eq!(outcome.version, "0.1.9-1");
        assert_eq!(fixture.daemon(), "DAEMON-ANTIGO");
        assert_eq!(fixture.helper(), "FW-ANTIGO");
    }

    #[test]
    fn tail_keeps_the_last_lines_in_one_line() {
        let output = CommandOutput {
            success: false,
            stdout: (1..=12)
                .map(|n| format!("linha {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            stderr: "erro de verdade".into(),
        };
        let text = tail(&output);
        assert!(text.contains("linha 12"));
        assert!(!text.contains("linha 1 "));
        assert!(!text.contains('\n'));
    }
}
