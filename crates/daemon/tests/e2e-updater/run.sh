#!/usr/bin/env bash
# Teste de ponta a ponta do helper root de atualização (`rustployd-update`), com systemd,
# dpkg, socket activation e permissões de verdade, num contêiner Podman.
#
# O que roda é o helper REAL (compilado aqui, com uma chave de teste no lugar da de produção, ver
# `trusted_keys()` em src/bin/rustployd-update/main.rs), as units e o postinst REAIS de
# packaging/. O "daemon" é um script que fica dormindo: o que se testa é o helper, não o rustployd.
#
#   crates/daemon/tests/e2e-updater/run.sh            # roda tudo (uns 4 minutos)
#   KEEP=1 crates/daemon/tests/e2e-updater/run.sh     # deixa o contêiner de pé para investigar
#
# Pré-requisitos: podman, minisign, socat, dpkg-deb e o toolchain do Rust.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$HERE/../../../.." && pwd)"          # rustploy-daemon/
PACKAGING="$WORKSPACE/packaging"
WORK="$(mktemp -d)"
CONTAINER="rustploy-e2e-updater"
IMAGE="localhost/rustploy-e2e-updater:latest"
FAILED=0

cleanup() {
    if [ -z "${KEEP:-}" ]; then podman rm -f "$CONTAINER" >/dev/null 2>&1 || true; fi
    rm -rf "$WORK"
}
trap cleanup EXIT

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32m✓\033[0m %s\n' "$*"; }
fail() { printf '  \033[31m✗ %s\033[0m\n' "$*"; FAILED=$((FAILED + 1)); }

# check <descrição> <esperado> <obtido>
check() {
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: esperado [$2], veio [$3]"; fi
}
# contains <descrição> <trecho> <texto>
contains() {
    case "$3" in *"$2"*) pass "$1" ;; *) fail "$1: [$2] não aparece em [$3]" ;; esac
}

in_container() { podman exec "$CONTAINER" "$@"; }
sh_in() { podman exec "$CONTAINER" sh -c "$1"; }

# ── chaves descartáveis ─────────────────────────────────────────────────────────
say "chaves de teste (descartáveis, só nesta pasta temporária)"
minisign -G -W -f -p "$WORK/e2e.pub" -s "$WORK/e2e.key" >/dev/null
minisign -G -W -f -p "$WORK/other.pub" -s "$WORK/other.key" >/dev/null
E2E_PUBLIC_KEY="$(sed -n 2p "$WORK/e2e.pub")"

# ── o helper real, com a chave de teste ───────────────────────────────────────────
say "compilando o helper com a chave de teste"
CARGO_EXTRA=()
if [ -d "$WORKSPACE/../rustploy-shared/update-core" ] && [ -n "${LOCAL_SHARED:-1}" ]; then
    CARGO_EXTRA=(--config "patch.crates-io.rustploy-shared.path=\"$WORKSPACE/../rustploy-shared\""
                 --config "patch.crates-io.rustploy-update-core.path=\"$WORKSPACE/../rustploy-shared/update-core\"")
fi
( cd "$WORKSPACE" && RUSTPLOY_E2E_TRUSTED_KEY="$E2E_PUBLIC_KEY" \
    cargo build -p rustploy --bin rustployd-update "${CARGO_EXTRA[@]}" >/dev/null 2>&1 )
HELPER="$WORKSPACE/target/debug/rustployd-update"
[ -x "$HELPER" ] || { echo "helper não compilou"; exit 1; }
pass "helper compilado: $HELPER"

# ── pacotes de mentira ──────────────────────────────────────────────────────────
# make_deb <arquivo> <pacote> <versão> <normal|postinst-quebrado|daemon-cai> <chave>
make_deb() {
    local out="$1" package="$2" version="$3" mode="$4" key="$5" dir
    dir="$(mktemp -d "$WORK/pkg.XXXXXX")"
    mkdir -p "$dir/DEBIAN" "$dir/usr/bin" "$dir/usr/libexec" "$dir/lib/systemd/system"
    {
        echo '#!/bin/sh'
        echo "# VERSION=$version"
        if [ "$mode" = "daemon-cai" ]; then
            echo 'exit 1'
        else
            echo "echo $version > /run/rustploy/running-version"
            echo 'exec sleep infinity'
        fi
    } > "$dir/usr/bin/rustployd"
    printf '#!/bin/sh\n# VERSION=%s\nexec sleep infinity\n' "$version" > "$dir/usr/libexec/rustployd-fw"
    cp "$HELPER" "$dir/usr/libexec/rustployd-update"
    chmod 755 "$dir/usr/bin/rustployd" "$dir/usr/libexec/rustployd-fw" "$dir/usr/libexec/rustployd-update"
    for unit in rustployd.service rustployd-fw.socket rustployd-fw.service \
                rustployd-update.socket rustployd-update.service; do
        cp "$PACKAGING/$unit" "$dir/lib/systemd/system/$unit"
    done
    cp "$PACKAGING/debian/postinst" "$dir/DEBIAN/postinst"
    [ "$mode" = "postinst-quebrado" ] && printf '\necho "postinst de teste falhando" >&2\nexit 1\n' >> "$dir/DEBIAN/postinst"
    chmod 755 "$dir/DEBIAN/postinst"
    cat > "$dir/DEBIAN/control" <<CONTROL
Package: $package
Version: $version
Architecture: amd64
Maintainer: e2e <e2e@example.invalid>
Description: pacote de mentira do teste de atualização
CONTROL
    dpkg-deb --build --root-owner-group "$dir" "$out" >/dev/null
    minisign -S -s "$key" -t "package=$package version=$version arch=amd64" -m "$out" >/dev/null
}

say "montando os pacotes"
mkdir -p "$WORK/debs"
make_deb "$WORK/debs/v1.deb"  rustployd 1.0.0-1 normal "$WORK/e2e.key"
make_deb "$WORK/debs/v2.deb"  rustployd 1.1.0-1 normal "$WORK/e2e.key"
make_deb "$WORK/debs/bad-sig.deb"  rustployd 1.4.0-1 normal "$WORK/other.key"
make_deb "$WORK/debs/other-pkg.deb" outro-pacote 1.5.0-1 normal "$WORK/e2e.key"
make_deb "$WORK/debs/downgrade.deb" rustployd 0.9.0-1 normal "$WORK/e2e.key"
make_deb "$WORK/debs/broken-postinst.deb" rustployd 1.2.0-1 postinst-quebrado "$WORK/e2e.key"
make_deb "$WORK/debs/crash.deb" rustployd 1.3.0-1 daemon-cai "$WORK/e2e.key"
make_deb "$WORK/debs/v8.deb" rustployd 1.6.0-1 normal "$WORK/e2e.key"
pass "8 pacotes assinados"

# ── o "servidor" ─────────────────────────────────────────────────────────────────
say "subindo o contêiner com systemd"
podman build -q -t "$IMAGE" -f "$HERE/Containerfile" "$HERE" >/dev/null
podman rm -f "$CONTAINER" >/dev/null 2>&1 || true
podman run -d --name "$CONTAINER" --systemd=always "$IMAGE" >/dev/null
for _ in $(seq 1 60); do
    state="$(podman exec "$CONTAINER" systemctl is-system-running 2>/dev/null || true)"
    case "$state" in running|degraded) break ;; esac
    sleep 1
done
check "systemd do contêiner subiu" "ok" "$([ "$state" = running ] || [ "$state" = degraded ] && echo ok || echo "$state")"

podman cp "$WORK/debs/v1.deb" "$CONTAINER:/tmp/v1.deb"
sh_in 'dpkg -i /tmp/v1.deb >/tmp/dpkg-v1.log 2>&1 || { cat /tmp/dpkg-v1.log; exit 1; }'

# ── helpers de cenário ───────────────────────────────────────────────────────────
installed_version() { sh_in "dpkg-query -W -f='\${Version}' rustployd"; }
running_version()   { sh_in 'cat /run/rustploy/running-version 2>/dev/null || echo ?'; }
service_state()     { sh_in 'systemctl is-active rustployd || true'; }
binary_marker()     { sh_in "grep -o 'VERSION=[^ ]*' /usr/bin/rustployd"; }
last_json_field()   { sh_in "grep -o '\"$1\": *\"[^\"]*\"' /var/lib/rustploy-update/last.json | sed 's/.*: *\"//; s/\"\$//'"; }

# Entrega o pacote como o daemon faria (usuário rustploy, em /var/lib/rustploy/updates) e pede a instalação.
send_install() {  # <nome-do-arquivo-local sem .deb>
    local name="$1"
    # Sem o last.json do cenário anterior, `wait_outcome` só enxerga o resultado deste pedido.
    sh_in 'rm -f /var/lib/rustploy-update/last.json; install -d -o rustploy -g rustploy /var/lib/rustploy/updates && find /var/lib/rustploy/updates -type f -delete'
    podman cp "$WORK/debs/$name.deb" "$CONTAINER:/var/lib/rustploy/updates/pkg.deb"
    podman cp "$WORK/debs/$name.deb.minisig" "$CONTAINER:/var/lib/rustploy/updates/pkg.deb.minisig"
    sh_in 'chown rustploy:rustploy /var/lib/rustploy/updates/*'
    send_as rustploy '{"op":"install","deb":"/var/lib/rustploy/updates/pkg.deb","sig":"/var/lib/rustploy/updates/pkg.deb.minisig"}'
}

send_as() {  # <usuário> <json>
    podman exec "$CONTAINER" su -s /bin/sh "$1" -c "echo '$2' | socat -t 10 - UNIX-CONNECT:/run/rustploy/update.sock 2>&1" || true
}

wait_outcome() {  # espera o last.json sair de "in_progress" (até 200 s)
    for _ in $(seq 1 200); do
        kind="$(last_json_field kind 2>/dev/null || true)"
        case "$kind" in installed|failed|rolled_back) return 0 ;; esac
        sleep 1
    done
    return 1
}

# ── cenários ─────────────────────────────────────────────────────────────────────
say "S0. estado inicial (pacote v1 instalado por dpkg -i)"
check "versão instalada" "1.0.0-1" "$(installed_version)"
check "o serviço do daemon está ativo" "active" "$(service_state)"
check "o socket do helper está ativo" "active" "$(sh_in 'systemctl is-active rustployd-update.socket')"
# O dono pode virar `rustploy`: o systemd refaz o chown do conteúdo de RuntimeDirectory=rustploy quando
# o rustployd sobe. Não importa (o helper não confia no cliente); o que vale é grupo e modo.
check "socket do grupo rustploy, modo 660 (ninguém mais acessa)" "rustploy 660" "$(sh_in "stat -c '%G %a' /run/rustploy/update.sock")"
check "o helper NÃO está residente (ativação por socket)" "inactive" "$(sh_in 'systemctl is-active rustployd-update.service || true')"

say "S1. controle de acesso do socket"
denied="$(send_as nobody '{"op":"rollback"}')"
contains "um usuário fora do grupo rustploy é barrado" "ermission denied" "$denied"

say "S2. caminho feliz: v1 -> v2"
reply="$(send_install v2)"
contains "o helper aceita (assinatura e regras conferem)" '"ok":true' "$reply"
wait_outcome || fail "a instalação não terminou"
check "resultado gravado" "installed" "$(last_json_field kind)"
check "versão instalada" "1.1.0-1" "$(installed_version)"
check "o serviço reiniciou na versão nova" "1.1.0-1" "$(running_version)"
check "o serviço está ativo e estável" "active" "$(service_state)"
check "backup guarda o binário anterior" "VERSION=1.0.0-1" "$(sh_in "grep -o 'VERSION=[^ ]*' /var/lib/rustploy-update/backup/rustployd")"
check "o helper saiu depois do pedido" "inactive" "$(sh_in 'systemctl is-active rustployd-update.service || true')"
check "a cópia de trabalho foi apagada" "0" "$(sh_in 'ls /var/lib/rustploy-update/staging 2>/dev/null | wc -l')"

say "S3. assinatura de OUTRA chave é recusada"
reply="$(send_install bad-sig)"
contains "recusa pela assinatura" "assinatura inválida" "$reply"
check "nada mudou" "1.1.0-1" "$(installed_version)"
check "resultado gravado" "failed" "$(last_json_field kind)"

say "S4. pacote assinado de OUTRO produto é recusado"
reply="$(send_install other-pkg)"
contains "recusa pelo nome do pacote" "pacote errado" "$reply"
check "nada mudou" "1.1.0-1" "$(installed_version)"

say "S5. downgrade é recusado"
reply="$(send_install downgrade)"
contains "recusa por não ser mais novo" "downgrade" "$reply"
check "nada mudou" "1.1.0-1" "$(installed_version)"

say "S6. arquivo fora do diretório de atualizações é recusado"
reply="$(send_as rustploy '{"op":"install","deb":"/etc/passwd","sig":"/etc/shadow"}')"
contains "recusa pelo caminho" "pacote recusado" "$reply"

say "S7. postinst que falha: os binários antigos voltam"
reply="$(send_install broken-postinst)"
contains "o helper aceita (o defeito só aparece ao instalar)" '"ok":true' "$reply"
wait_outcome || fail "a instalação não terminou"
check "resultado gravado" "failed" "$(last_json_field kind)"
contains "a causa do dpkg está na mensagem" "dpkg falhou" "$(last_json_field message)"
check "o binário do daemon voltou ao anterior" "VERSION=1.1.0-1" "$(binary_marker)"
check "o serviço está ativo" "active" "$(service_state)"

say "S8. daemon novo que cai em laço: volta atrás sozinho (leva ~1,5 min)"
reply="$(send_install crash)"
contains "o helper aceita" '"ok":true' "$reply"
wait_outcome || fail "a instalação não terminou"
check "resultado gravado" "rolled_back" "$(last_json_field kind)"
contains "mensagem diz que não ficou estável" "não ficou estável" "$(last_json_field message)"
check "o binário do daemon voltou ao anterior" "VERSION=1.1.0-1" "$(binary_marker)"
for _ in $(seq 1 30); do [ "$(service_state)" = active ] && break; sleep 1; done
check "o serviço voltou a ficar ativo" "active" "$(service_state)"

say "S9. nova instalação boa depois de falhas"
reply="$(send_install v8)"
contains "o helper aceita" '"ok":true' "$reply"
wait_outcome || fail "a instalação não terminou"
check "resultado gravado" "installed" "$(last_json_field kind)"
check "versão instalada" "1.6.0-1" "$(installed_version)"
check "o serviço roda a versão nova" "1.6.0-1" "$(running_version)"

say "S10. rollback manual"
sh_in 'rm -f /var/lib/rustploy-update/last.json'
reply="$(send_as rustploy '{"op":"rollback"}')"
contains "o helper aceita antes de reiniciar o daemon" '"ok":true' "$reply"
wait_outcome || fail "o rollback não terminou"
check "o binário do daemon voltou ao do backup" "VERSION=1.1.0-1" "$(binary_marker)"
check "o serviço está ativo" "active" "$(service_state)"
check "resultado gravado" "rolled_back" "$(last_json_field kind)"

say "S11. rollback sem backup é recusado"
sh_in 'rm -rf /var/lib/rustploy-update/backup'
reply="$(send_as rustploy '{"op":"rollback"}')"
contains "recusa dizendo que não há backup" "não há backup" "$reply"

say "resumo"
if [ "$FAILED" -eq 0 ]; then
    printf '\033[32mtodos os cenários passaram\033[0m\n'
else
    printf '\033[31m%s verificação(ões) falharam\033[0m\n' "$FAILED"
    [ -n "${KEEP:-}" ] || podman logs "$CONTAINER" 2>&1 | tail -5
    exit 1
fi
