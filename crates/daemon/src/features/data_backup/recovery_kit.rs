//! Arquivo de recuperação (`.rpkit`): a chave privada de recuperação junto com o que a tela Recuperar
//! pede — repositório da infra, branch e token —, num formato binário que os clientes leem sozinhos.
//! Plano: `docs/plano-arquivo-de-recuperacao.md`.
//!
//! # Formato (versão 1)
//!
//! Inteiros em little-endian. Um cabeçalho fixo, os campos e uma soma de verificação:
//!
//! ```text
//! offset  bytes  conteúdo
//! 0       8      assinatura: 89 'R' 'P' 'K' 'I' 'T' 0D 0A
//! 8       2      versão (u16 = 1)
//! 10      2      quantidade de campos (u16 = N)
//! 12      …      N campos: etiqueta (u16) | tamanho (u32) | dados (UTF-8)
//! fim-4   4      CRC-32 (IEEE) de tudo o que vem antes (u32)
//! ```
//!
//! A assinatura é a mesma ideia da do PNG: o `0x89` denuncia uma cópia que zerou o bit alto (transferência
//! em modo texto) e o `0D 0A`, uma conversão de fim de linha. Qualquer byte mudado, faltando ou sobrando
//! é pego pelo CRC: o arquivo não se corrompe em silêncio nas mãos de quem o abriu num editor.
//!
//! Etiqueta desconhecida é pulada na leitura: uma versão futura pode acrescentar campos sem quebrar os
//! clientes de hoje. Os mesmos leitores existem em Luau (`rustploy-gui/views/scripts/recovery_kit.luau`)
//! e em JS (`webui/recovery_kit.js`): mude os três juntos. Vai na resposta JSON em base64.
//!
//! **Não é cifrado.** Quem tem o arquivo tem a chave e o token; o formato protege contra corrupção, não
//! contra leitura.

use anyhow::{Context, Result, bail};

/// A assinatura dos 8 primeiros bytes.
pub const MAGIC: [u8; 8] = [0x89, b'R', b'P', b'K', b'I', b'T', 0x0D, 0x0A];
/// A versão que esta build escreve (e a única que lê).
pub const VERSION: u16 = 1;
/// Teto de um campo: nada aqui passa de alguns KB; um tamanho absurdo é corrupção.
const MAX_FIELD: usize = 64 * 1024;
/// Assinatura + versão + contagem.
const HEADER_LEN: usize = MAGIC.len() + 2 + 2;
/// Etiqueta + tamanho.
const FIELD_HEADER_LEN: usize = 2 + 4;

const TAG_INFRA_REMOTE: u16 = 1;
const TAG_INFRA_BRANCH: u16 = 2;
const TAG_INFRA_TOKEN: u16 = 3;
const TAG_PRIVATE_KEY: u16 = 4;
const TAG_PUBLIC_KEY: u16 = 5;
const TAG_SERVER: u16 = 6;
const TAG_CREATED_AT: u16 = 7;

/// O conteúdo de um arquivo de recuperação.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryKit {
    /// `https://…` do repositório da infra (o do Git Sync).
    pub infra_remote: String,
    /// Branch; vazio = a padrão do repositório.
    pub infra_branch: String,
    /// Token de acesso ao repositório (vazio só para remoto local, `file://`).
    pub infra_token: String,
    /// `AGE-SECRET-KEY-1…`.
    pub private_key: String,
    /// `age1…`, para a tela avisar quando o arquivo é de uma chave anterior.
    pub public_key: String,
    /// O servidor que gerou o arquivo.
    pub server: String,
    /// Quando foi gerado (RFC 3339).
    pub created_at: String,
}

/// CRC-32 (IEEE 802.3, o do zip e do PNG), bit a bit: sem tabela e sem crate.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// `u16` little-endian em `bytes[at..at+2]`.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// `u32` little-endian em `bytes[at..at+4]`.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl RecoveryKit {
    /// Os bytes do arquivo.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let fields: [(u16, &str); 7] = [
            (TAG_INFRA_REMOTE, &self.infra_remote),
            (TAG_INFRA_BRANCH, &self.infra_branch),
            (TAG_INFRA_TOKEN, &self.infra_token),
            (TAG_PRIVATE_KEY, &self.private_key),
            (TAG_PUBLIC_KEY, &self.public_key),
            (TAG_SERVER, &self.server),
            (TAG_CREATED_AT, &self.created_at),
        ];
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (tag, value) in fields {
            if value.len() > MAX_FIELD {
                bail!(
                    "campo {tag} do arquivo de recuperação grande demais ({} bytes)",
                    value.len()
                );
            }
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        let checksum = crc32(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        Ok(out)
    }

    /// Lê e confere um arquivo. O erro diz o que houve, em palavras de quem vai usar a tela.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN + 4 || bytes[1..6] != MAGIC[1..6] {
            bail!("não é um arquivo de recuperação do Rustploy");
        }
        if bytes[..MAGIC.len()] != MAGIC {
            bail!(
                "o arquivo de recuperação foi alterado na cópia (modo texto ou conversão de fim de \
                 linha): use o arquivo original"
            );
        }
        let (body, tail) = bytes.split_at(bytes.len() - 4);
        let stored = u32_at(tail, 0).context("o arquivo de recuperação está truncado")?;
        if stored != crc32(body) {
            bail!("o arquivo de recuperação está corrompido (a soma de verificação não confere)");
        }
        let version = u16_at(body, 8).context("o arquivo de recuperação está truncado")?;
        if version != VERSION {
            bail!(
                "arquivo de recuperação da versão {version}; esta versão do Rustploy lê a {VERSION}"
            );
        }
        let count = u16_at(body, 10).context("o arquivo de recuperação está truncado")?;

        let mut kit = RecoveryKit::default();
        let mut seen_remote = false;
        let mut position = HEADER_LEN;
        for _ in 0..count {
            let tag = u16_at(body, position).context("o arquivo de recuperação está truncado")?;
            let length = u32_at(body, position + 2)
                .context("o arquivo de recuperação está truncado")?
                as usize;
            if length > MAX_FIELD {
                bail!("o arquivo de recuperação está corrompido (campo grande demais)");
            }
            let start = position + FIELD_HEADER_LEN;
            let data = body
                .get(start..start + length)
                .context("o arquivo de recuperação está truncado")?;
            let text = std::str::from_utf8(data)
                .context("o arquivo de recuperação está corrompido (texto inválido)")?
                .to_string();
            match tag {
                TAG_INFRA_REMOTE => {
                    kit.infra_remote = text;
                    seen_remote = true;
                }
                TAG_INFRA_BRANCH => kit.infra_branch = text,
                TAG_INFRA_TOKEN => kit.infra_token = text,
                TAG_PRIVATE_KEY => kit.private_key = text,
                TAG_PUBLIC_KEY => kit.public_key = text,
                TAG_SERVER => kit.server = text,
                TAG_CREATED_AT => kit.created_at = text,
                _ => {} // campo de uma versão futura
            }
            position = start + length;
        }
        if position != body.len() {
            bail!("o arquivo de recuperação está corrompido (sobram bytes)");
        }
        if !seen_remote || kit.infra_remote.is_empty() || kit.private_key.is_empty() {
            bail!("o arquivo de recuperação está incompleto (falta o repositório ou a chave)");
        }
        Ok(kit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RecoveryKit {
        RecoveryKit {
            infra_remote: "https://github.com/org/infra.git".into(),
            infra_branch: "main".into(),
            infra_token: "ghp_exemplo".into(),
            private_key: "AGE-SECRET-KEY-1EXEMPLO".into(),
            public_key: "age1exemplo".into(),
            server: "vps-ção".into(),
            created_at: "2026-11-18T12:00:00Z".into(),
        }
    }

    #[test]
    fn crc32_bate_com_o_valor_de_referencia() {
        // O "check value" do CRC-32/IEEE.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn ida_e_volta() {
        let kit = sample();
        assert_eq!(RecoveryKit::decode(&kit.encode().unwrap()).unwrap(), kit);
    }

    #[test]
    fn e_binario_de_verdade() {
        let bytes = sample().encode().unwrap();
        assert_eq!(&bytes[..8], &MAGIC);
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "o 0x89 da assinatura não é UTF-8"
        );
    }

    #[test]
    fn copia_em_modo_texto_tem_mensagem_propria() {
        let mut bytes = sample().encode().unwrap();
        bytes[0] &= 0x7F; // bit alto zerado
        let error = RecoveryKit::decode(&bytes).unwrap_err().to_string();
        assert!(error.contains("modo texto"), "{error}");
    }

    #[test]
    fn qualquer_byte_mudado_e_recusado() {
        let bytes = sample().encode().unwrap();
        for index in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[index] ^= 0x01;
            assert!(
                RecoveryKit::decode(&damaged).is_err(),
                "byte {index} mudado passou"
            );
        }
    }

    #[test]
    fn truncado_ou_com_sobra_e_recusado() {
        let bytes = sample().encode().unwrap();
        assert!(RecoveryKit::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(b'\n');
        assert!(RecoveryKit::decode(&longer).is_err());
    }

    #[test]
    fn conversao_de_fim_de_linha_tem_mensagem_propria() {
        let mut bytes = sample().encode().unwrap();
        bytes.remove(6); // \r\n → \n
        let error = RecoveryKit::decode(&bytes).unwrap_err().to_string();
        assert!(error.contains("fim de linha"), "{error}");
    }

    #[test]
    fn texto_qualquer_nao_e_arquivo_de_recuperacao() {
        let error = RecoveryKit::decode(b"AGE-SECRET-KEY-1ABC\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("não é um arquivo"), "{error}");
    }

    /// O vetor que os leitores em Luau e JS conferem: se mudar, mude os testes deles junto.
    #[test]
    fn vetor_compartilhado_com_os_clientes() {
        let kit = RecoveryKit {
            infra_remote: "https://g.io/i.git".into(),
            infra_branch: "main".into(),
            infra_token: "tok".into(),
            private_key: "AGE-SECRET-KEY-1X".into(),
            public_key: "age1x".into(),
            server: "vps".into(),
            created_at: "2026-11-18T12:00:00Z".into(),
        };
        let hex: String = kit
            .encode()
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, SHARED_VECTOR_HEX);
    }

    const SHARED_VECTOR_HEX: &str = "8952504b49540d0a0100070001001200000068747470733a2f2f672e696f2f692e67697402000400\
        00006d61696e030003000000746f6b0400110000004147452d5345435245542d4b45592d31580500\
        050000006167653178060003000000767073070014000000323032362d31312d31385431323a3030\
        3a30305a1837508b";
}
