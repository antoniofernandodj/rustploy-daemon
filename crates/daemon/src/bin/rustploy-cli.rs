//! `rustploy-cli`: ferramentas de linha de comando que **não precisam do daemon rodando**.
//!
//! Hoje serve ao fluxo de segredos do plano `docs/plano-backup-estado-em-git.md` (R5/R6): dá para
//! editar o YAML de infra à mão (ou pelo git) numa máquina que não tem a chave privada, cifrando o
//! valor só com a chave **pública** do servidor.
//!
//! ```text
//! rustploy-cli keygen                         gera uma identidade age (privada + pública)
//! rustploy-cli secret encrypt [-r age1…]…     lê o valor do stdin e imprime ENC[age:…]
//! rustploy-cli secret decrypt [ENC[age:…]]    decifra (identidade via -i/--identity-file ou config)
//! ```
//! O valor **nunca** é aceito como argumento (iria para o histórico do shell e para o `ps`).

// O formato é o mesmo código que o daemon usa para ler: o que o CLI cifra o daemon decifra.
#[path = "../platform/age_format.rs"]
mod age_format;

use age::secrecy::ExposeSecret;
use age::x25519;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

const DEFAULT_CONFIG: &str = "/etc/rustploy/config.toml";

const USAGE: &str = "rustploy-cli — ferramentas de segredos do rustploy

USO:
  rustploy-cli keygen
  rustploy-cli secret encrypt [-r|--recipient age1…]… [-c|--config ARQUIVO]
  rustploy-cli secret decrypt [ENC[age:…]] [-i|--identity-file ARQUIVO] [-c|--config ARQUIVO]

O valor a cifrar vem do stdin (ou é pedido no terminal) — nunca como argumento.
Sem --recipient, os destinatários vêm do config.toml ([secrets] age_identity e
recovery_recipient; padrão /etc/rustploy/config.toml, que só o dono lê).
Para decifrar: --identity-file, a variável RUSTPLOY_AGE_IDENTITY, ou o config.toml.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
        }
        Err(e) => {
            eprintln!("erro: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(args: &[String]) -> Result<String> {
    match args.first().map(String::as_str) {
        Some("keygen") => Ok(keygen()),
        Some("secret") => match args.get(1).map(String::as_str) {
            Some("encrypt") => secret_encrypt(&args[2..]),
            Some("decrypt") => secret_decrypt(&args[2..]),
            _ => bail!("{USAGE}"),
        },
        Some("-h" | "--help" | "help") | None => Ok(USAGE.trim_end().to_string()),
        Some(other) => bail!("comando desconhecido: {other}\n\n{USAGE}"),
    }
}

fn keygen() -> String {
    let id = x25519::Identity::generate();
    format!(
        "# chave pública: {}\n{}",
        id.to_public(),
        id.to_string().expose_secret()
    )
}

/// Opções comuns, tiradas dos argumentos (`-r X`, `--recipient X`, `--recipient=X`…).
#[derive(Default, Debug, PartialEq)]
struct Opts {
    recipients: Vec<String>,
    identity_file: Option<PathBuf>,
    config: Option<PathBuf>,
    positional: Vec<String>,
}

fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| anyhow!("{name} precisa de um valor"))
        };
        match flag {
            "-r" | "--recipient" => o.recipients.push(value("--recipient")?),
            "-i" | "--identity-file" => o.identity_file = Some(value("--identity-file")?.into()),
            "-c" | "--config" => o.config = Some(value("--config")?.into()),
            f if f.starts_with('-') && f != "-" => bail!("opção desconhecida: {f}"),
            _ => o.positional.push(a.clone()),
        }
    }
    Ok(o)
}

/// Lê `[secrets]` do config.toml: `(age_identity, recovery_recipient)`.
fn read_secrets_section(path: &std::path::Path) -> Result<(Option<String>, Option<String>)> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "não consegui ler {} (o config.toml só é legível pelo dono; rode como esse usuário, ou passe --recipient / --identity-file)",
            path.display()
        )
    })?;
    let doc: toml::Value =
        toml::from_str(&text).with_context(|| format!("{} inválido", path.display()))?;
    let get = |k: &str| {
        doc.get("secrets")
            .and_then(|s| s.get(k))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok((get("age_identity"), get("recovery_recipient")))
}

fn config_path(o: &Opts) -> PathBuf {
    o.config
        .clone()
        .or_else(|| std::env::var_os("RUSTPLOY_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| DEFAULT_CONFIG.into())
}

/// Os destinatários: os de `--recipient`, ou os do config (chave pública do servidor + recuperação).
fn resolve_recipients(o: &Opts) -> Result<Vec<x25519::Recipient>> {
    if !o.recipients.is_empty() {
        return o
            .recipients
            .iter()
            .map(|r| {
                age_format::parse_recipient(r)
                    .with_context(|| format!("destinatário inválido: {r}"))
            })
            .collect();
    }
    let (identity, recovery) = read_secrets_section(&config_path(o))?;
    let mut out = vec![];
    if let Some(id) = identity {
        out.push(
            age_format::parse_identity(&id)
                .context("[secrets] age_identity inválida")?
                .to_public(),
        );
    }
    if let Some(r) = recovery {
        out.push(age_format::parse_recipient(&r).context("[secrets] recovery_recipient inválida")?);
    }
    if out.is_empty() {
        bail!("nenhum destinatário: passe --recipient age1… ou configure [secrets] no config.toml");
    }
    Ok(out)
}

fn resolve_identity(o: &Opts) -> Result<x25519::Identity> {
    if let Some(f) = &o.identity_file {
        let text = std::fs::read_to_string(f).with_context(|| format!("lendo {}", f.display()))?;
        // aceita o arquivo do `keygen` (com a linha de comentário) ou só a chave
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .ok_or_else(|| anyhow!("{} não tem uma linha AGE-SECRET-KEY-…", f.display()))?;
        return age_format::parse_identity(line);
    }
    if let Ok(v) = std::env::var("RUSTPLOY_AGE_IDENTITY")
        && !v.trim().is_empty()
    {
        return age_format::parse_identity(&v).context("RUSTPLOY_AGE_IDENTITY inválida");
    }
    let (identity, _) = read_secrets_section(&config_path(o))?;
    let id = identity.ok_or_else(|| {
        anyhow!(
            "sem identidade: use --identity-file, RUSTPLOY_AGE_IDENTITY ou [secrets] age_identity"
        )
    })?;
    age_format::parse_identity(&id).context("[secrets] age_identity inválida")
}

/// O valor sensível: do stdin; se for um terminal, pede (sem eco, quando o `stty` existe).
fn read_secret_value() -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("valor (não aparece enquanto você digita): ");
        let _ = std::io::stderr().flush();
        let _ = std::process::Command::new("stty").arg("-echo").status();
        let mut line = String::new();
        let r = stdin.read_line(&mut line);
        let _ = std::process::Command::new("stty").arg("echo").status();
        eprintln!();
        r?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_string());
    }
    let mut v = String::new();
    stdin.lock().read_to_string(&mut v)?;
    // um único "\n" final é do `echo`, não do valor
    Ok(v.strip_suffix('\n').unwrap_or(&v).to_string())
}

fn secret_encrypt(args: &[String]) -> Result<String> {
    let o = parse_opts(args)?;
    if !o.positional.is_empty() {
        bail!(
            "o valor NÃO é aceito como argumento (iria para o histórico do shell): passe pelo stdin"
        );
    }
    let recipients = resolve_recipients(&o)?;
    let value = read_secret_value()?;
    if value.is_empty() {
        bail!("valor vazio");
    }
    age_format::encrypt(&value, &recipients)
}

fn secret_decrypt(args: &[String]) -> Result<String> {
    let o = parse_opts(args)?;
    let identity = resolve_identity(&o)?;
    let value = match o.positional.as_slice() {
        [v] => v.clone(),
        [] => {
            let mut v = String::new();
            std::io::stdin().lock().read_to_string(&mut v)?;
            v.trim().to_string()
        }
        _ => bail!("passe um único valor ENC[age:…]"),
    };
    age_format::decrypt(&value, &identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn keygen_gera_par_valido() {
        let out = keygen();
        let sk = out
            .lines()
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .unwrap();
        let id = age_format::parse_identity(sk).unwrap();
        assert!(out.contains(&id.to_public().to_string()));
    }

    #[test]
    fn opcoes_em_varias_formas() {
        let o = parse_opts(&s(&[
            "-r",
            "age1a",
            "--recipient=age1b",
            "-c",
            "/x.toml",
            "ENC[age:x]",
        ]))
        .unwrap();
        assert_eq!(o.recipients, ["age1a", "age1b"]);
        assert_eq!(o.config, Some("/x.toml".into()));
        assert_eq!(o.positional, ["ENC[age:x]"]);
        assert!(parse_opts(&s(&["--nada"])).is_err());
        assert!(parse_opts(&s(&["-r"])).is_err());
    }

    #[test]
    fn valor_como_argumento_e_recusado() {
        let e = secret_encrypt(&s(&["minha-senha"]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("NÃO é aceito como argumento"), "{e}");
    }

    #[test]
    fn destinatarios_vem_do_config_com_a_recuperacao() {
        let (srv, rec) = (x25519::Identity::generate(), x25519::Identity::generate());
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "[secrets]\nmaster_key_path = \"/x\"\nage_identity = \"{}\"\nrecovery_recipient = \"{}\"\n",
                srv.to_string().expose_secret(),
                rec.to_public()
            ),
        )
        .unwrap();
        let o = Opts {
            config: Some(cfg.clone()),
            ..Default::default()
        };
        let rs = resolve_recipients(&o).unwrap();
        assert_eq!(rs.len(), 2);
        let c = age_format::encrypt("v", &rs).unwrap();
        // o servidor e a recuperação leem; quem não está na lista, não
        assert_eq!(age_format::decrypt(&c, &srv).unwrap(), "v");
        assert_eq!(age_format::decrypt(&c, &rec).unwrap(), "v");
        // a identidade vem do config quando não há arquivo
        let o2 = Opts {
            config: Some(cfg),
            ..Default::default()
        };
        assert_eq!(
            age_format::decrypt(&c, &resolve_identity(&o2).unwrap()).unwrap(),
            "v"
        );
    }

    #[test]
    fn identidade_do_arquivo_do_keygen_com_comentario() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("id.txt");
        std::fs::write(&f, keygen() + "\n").unwrap();
        let o = Opts {
            identity_file: Some(f),
            ..Default::default()
        };
        assert!(resolve_identity(&o).is_ok());
    }

    #[test]
    fn sem_destinatario_da_erro_claro() {
        let o = Opts {
            config: Some("/nao/existe.toml".into()),
            ..Default::default()
        };
        let e = format!("{:#}", resolve_recipients(&o).unwrap_err());
        assert!(e.contains("não consegui ler"), "{e}");
    }
}
