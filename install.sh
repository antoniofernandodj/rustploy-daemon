#!/bin/sh
# Obsoleto. O instalador agora fica no repositório agregador e baixa os pacotes .deb da
# última release do GitHub:
#   curl -fsSL https://raw.githubusercontent.com/antoniofernandodj/rustploy/main/install.sh | sudo sh
# Este arquivo só repassa para ele, para quem ainda tem o endereço antigo.
exec sh -c "$(curl -fsSL https://raw.githubusercontent.com/antoniofernandodj/rustploy/main/install.sh)" install.sh "$@"
