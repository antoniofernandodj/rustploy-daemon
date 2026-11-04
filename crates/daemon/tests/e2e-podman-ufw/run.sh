#!/bin/sh
# Teste ponta a ponta do rustploy-podman-ufw: ufw + iptables-nft REAIS num laboratório de redes dentro de um
# contêiner Podman rootless privilegiado (não precisa de root nem toca no firewall do host). ~1 minuto.
set -eu
AQUI="$(cd "$(dirname "$0")" && pwd)"
HELPER="$AQUI/../../../../packaging/rustploy-podman-ufw"
exec podman run --rm --privileged \
    -v "$AQUI/lab.sh:/lab.sh:ro" -v "$HELPER:/helper:ro" \
    docker.io/library/debian:12 sh /lab.sh
