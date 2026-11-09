//! O pacote `.rpdata`: um tar aberto com `manifest.json` **primeiro**, depois
//! `parts/<arquivo>`, um por parte (tar de volume ou dump de banco). Cada parte leva o SHA-256, conferido antes de aplicar.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const MAX_MANIFEST: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestPart {
    /// Nome lógico (casa com `Part::name` na importação).
    pub name: String,
    /// Nome do arquivo dentro do pacote (`parts/<file>`); nunca vem do nome lógico.
    pub file: String,
    pub size: u64,
    pub sha256: String,
    /// `"volume"` (tar do volume) ou `"database"` (dump lógico).
    #[serde(default = "kind_volume")]
    pub kind: String,
    /// Só `database`: o motor (`postgres`, `mysql`, `mariadb`, `mongodb`).
    #[serde(default)]
    pub engine: Option<String>,
    /// Só `database`: a imagem do servidor de origem (`postgres:18`), para recusar versão mais velha.
    #[serde(default)]
    pub version: Option<String>,
    /// Só `database`: `tabela|contagem` por linha, para conferir depois de restaurar.
    #[serde(default)]
    pub counts: Option<String>,
}

fn kind_volume() -> String {
    "volume".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: u32,
    pub rustploy_version: String,
    pub service_name: String,
    pub created_at: String,
    pub parts: Vec<ManifestPart>,
}

/// Uma parte já tirada do volume, esperando entrar no pacote.
pub struct StagedPart {
    pub name: String,
    pub path: PathBuf,
    pub kind: String,
    pub engine: Option<String>,
    pub version: Option<String>,
    pub counts: Option<String>,
}

impl StagedPart {
    pub fn volume(name: &str, path: PathBuf) -> Self {
        Self {
            name: name.into(),
            path,
            kind: "volume".into(),
            engine: None,
            version: None,
            counts: None,
        }
    }
}

/// SHA-256 e tamanho de um arquivo, lendo em blocos.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(h.finalize()), total))
}

/// Monta o pacote em `out`. Bloqueante: chame de `spawn_blocking`.
pub fn write_pack(service_name: &str, staged: &[StagedPart], out: &Path) -> Result<Manifest> {
    let mut parts = Vec::new();
    for (i, s) in staged.iter().enumerate() {
        let (sha256, size) = hash_file(&s.path)?;
        parts.push(ManifestPart {
            name: s.name.clone(),
            file: format!("part{i}.tar"),
            size,
            sha256,
            kind: s.kind.clone(),
            engine: s.engine.clone(),
            version: s.version.clone(),
            counts: s.counts.clone(),
        });
    }
    let manifest = Manifest {
        format: FORMAT,
        rustploy_version: env!("CARGO_PKG_VERSION").to_string(),
        service_name: service_name.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        parts,
    };
    let json = serde_json::to_vec_pretty(&manifest)?;
    let mut b = tar::Builder::new(File::create(out)?);
    let mut h = tar::Header::new_gnu();
    h.set_size(json.len() as u64);
    h.set_mode(0o600);
    h.set_cksum();
    b.append_data(&mut h, MANIFEST, json.as_slice())?;
    for (s, p) in staged.iter().zip(&manifest.parts) {
        b.append_path_with_name(&s.path, format!("parts/{}", p.file))?;
    }
    b.into_inner()?.flush()?;
    Ok(manifest)
}

/// Lê só o manifesto (primeira entrada), sem extrair nada.
pub fn read_manifest(pack: &Path) -> Result<Manifest> {
    let mut ar = tar::Archive::new(File::open(pack)?);
    let mut entries = ar.entries()?;
    let mut first = entries
        .next()
        .ok_or_else(|| anyhow!("pacote vazio"))?
        .context("pacote ilegível")?;
    if first.path()?.to_string_lossy() != MANIFEST {
        bail!("não é um pacote de dados do Rustploy (falta o manifest.json na frente)");
    }
    if first.size() > MAX_MANIFEST {
        bail!("manifest.json grande demais");
    }
    let mut buf = Vec::new();
    first.read_to_end(&mut buf)?;
    let m: Manifest = serde_json::from_slice(&buf).context("manifest.json inválido")?;
    if m.format != FORMAT {
        bail!(
            "formato de pacote {} não suportado (este Rustploy entende o {FORMAT})",
            m.format
        );
    }
    Ok(m)
}

/// Extrai as partes do pacote para `dest` (arquivos `dest/<file>`), conferindo SHA-256 e tamanho de
/// cada uma. Só aceita as entradas que o manifesto lista, com o nome exato: nenhum caminho do
/// pacote vira caminho no disco. Bloqueante.
pub fn extract_verified(pack: &Path, dest: &Path) -> Result<Manifest> {
    let manifest = read_manifest(pack)?;
    let mut ar = tar::Archive::new(File::open(pack)?);
    let mut seen = std::collections::HashSet::new();
    for entry in ar.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if name == MANIFEST {
            continue;
        }
        let Some(part) = manifest
            .parts
            .iter()
            .find(|p| name == format!("parts/{}", p.file))
        else {
            bail!("entrada inesperada no pacote: {name}");
        };
        if !seen.insert(part.file.clone()) {
            bail!("entrada repetida no pacote: {name}");
        }
        let target = dest.join(&part.file);
        let mut out = File::create(&target)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            total += n as u64;
        }
        if total != part.size || hex::encode(h.finalize()) != part.sha256 {
            bail!(
                "a parte `{}` está corrompida (SHA-256 não confere)",
                part.name
            );
        }
    }
    for p in &manifest.parts {
        if !seen.contains(&p.file) {
            bail!("o pacote está incompleto: falta a parte `{}`", p.name);
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(dir: &Path, name: &str, body: &[u8]) -> StagedPart {
        let path = dir.join(format!("{}.raw", name.replace(['/', ':'], "_")));
        std::fs::write(&path, body).unwrap();
        StagedPart::volume(name, path)
    }

    #[test]
    fn ida_e_volta_confere_o_conteudo() {
        let d = tempfile::tempdir().unwrap();
        let s = vec![
            staged(d.path(), "pgdata", b"dados do banco"),
            staged(d.path(), "bind:/uploads", b"arquivos"),
        ];
        let pack = d.path().join("x.rpdata");
        let m = write_pack("db", &s, &pack).unwrap();
        assert_eq!(m.parts.len(), 2);
        assert_eq!(read_manifest(&pack).unwrap().service_name, "db");

        let out = d.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let m2 = extract_verified(&pack, &out).unwrap();
        assert_eq!(m, m2);
        assert_eq!(
            std::fs::read(out.join("part0.tar")).unwrap(),
            b"dados do banco"
        );
        assert_eq!(std::fs::read(out.join("part1.tar")).unwrap(), b"arquivos");
    }

    #[test]
    fn parte_adulterada_e_recusada() {
        let d = tempfile::tempdir().unwrap();
        let s = vec![staged(d.path(), "v", b"original-original")];
        let pack = d.path().join("x.rpdata");
        write_pack("svc", &s, &pack).unwrap();
        // troca um byte do conteúdo dentro do tar (o texto aparece uma vez, em claro)
        let mut bytes = std::fs::read(&pack).unwrap();
        let at = bytes
            .windows(8)
            .position(|w| w == b"original")
            .expect("conteúdo no tar");
        bytes[at] = b'X';
        std::fs::write(&pack, bytes).unwrap();
        let out = d.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let err = extract_verified(&pack, &out).unwrap_err().to_string();
        assert!(err.contains("corrompida"), "{err}");
    }

    #[test]
    fn arquivo_qualquer_nao_e_pacote() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("lixo");
        std::fs::write(&p, b"nao sou um tar").unwrap();
        assert!(read_manifest(&p).is_err());
    }

    #[test]
    fn entrada_fora_do_manifesto_e_recusada() {
        let d = tempfile::tempdir().unwrap();
        let s = vec![staged(d.path(), "v", b"x")];
        let ok = d.path().join("ok.rpdata");
        let m = write_pack("svc", &s, &ok).unwrap();
        // refaz o tar com uma entrada extra `../../evil`
        let bad = d.path().join("bad.rpdata");
        let json = serde_json::to_vec(&m).unwrap();
        let mut b = tar::Builder::new(File::create(&bad).unwrap());
        let mut h = tar::Header::new_gnu();
        h.set_size(json.len() as u64);
        h.set_cksum();
        b.append_data(&mut h, MANIFEST, json.as_slice()).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o600);
        h.set_cksum();
        b.append_data(&mut h, "parts/evil.tar", &b"x"[..]).unwrap();
        b.into_inner().unwrap();
        let out = d.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let err = extract_verified(&bad, &out).unwrap_err().to_string();
        assert!(err.contains("inesperada"), "{err}");
    }
}
