#!/bin/sh
# Laboratório do rustploy-podman-ufw (roda DENTRO de um contêiner Podman rootless privilegiado; use o run.sh).
# Monta, em namespaces de rede, um "servidor" com ufw + iptables-nft reais, duas redes de projeto (bridges, como o
# netavark) e uma "internet", e confere com sondas TCP o que o auxiliar libera e bloqueia.
set -u
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq ufw iptables iproute2 netcat-openbsd procps util-linux >/dev/null 2>&1 || { echo "FALHA: apt"; exit 2; }
echo "$(iptables --version | head -1) / $(ufw version | head -1)"
sysctl -qw net.ipv4.ip_forward=1 || echo "aviso: ip_forward"

ip link add brA type bridge && ip addr add 10.89.0.1/24 dev brA && ip link set brA up
ip link add brB type bridge && ip addr add 10.89.1.1/24 dev brB && ip link set brB up
mkns() { unshare -n sleep 100000 & echo $! > /run/ns.$1; sleep 0.3; }
inns() { n=$1; shift; nsenter -t "$(cat /run/ns.$n)" -n "$@"; }
mk() { ns=$1; br=$2; ip=$3; gw=$4
  mkns $ns
  ip link add v$ns type veth peer name p$ns || return 1
  ip link set p$ns netns "$(cat /run/ns.$ns)"; ip link set v$ns master $br; ip link set v$ns up
  inns $ns ip addr add $ip/24 dev p$ns; inns $ns ip link set p$ns up; inns $ns ip link set lo up
  inns $ns ip route add default via $gw; }
mk a1 brA 10.89.0.2 10.89.0.1 || { echo "FALHA: criar veth/namespace"; exit 2; }
mk a2 brA 10.89.0.3 10.89.0.1
mk b1 brB 10.89.1.2 10.89.1.1
mkns ext
ip link add vext type veth peer name pext; ip link set pext netns "$(cat /run/ns.ext)"
ip addr add 192.0.2.1/24 dev vext; ip link set vext up
inns ext ip addr add 192.0.2.2/24 dev pext; inns ext ip link set pext up; inns ext ip link set lo up
inns ext ip route add 10.89.0.0/16 via 192.0.2.1
(inns ext nc -lk 192.0.2.2 443  >/dev/null 2>&1 &)
(inns b1  nc -lk 10.89.1.2 5432 >/dev/null 2>&1 &)
(inns a2  nc -lk 10.89.0.3 6379 >/dev/null 2>&1 &)
(nc -lk 10.89.0.1 53 >/dev/null 2>&1 &)   # "aardvark-dns" no endereço do host da bridge A
(nc -lk 10.89.1.1 53 >/dev/null 2>&1 &)   # idem na B
sleep 1

FALHAS=0
probe() { if inns "$1" nc -z -w2 "$2" "$3" >/dev/null 2>&1; then echo OK; else echo BLOQ; fi; }
# confere FASE dns saida mesma_rede outro_projeto   (OK ou BLOQ para cada)
confere() {
  fase=$1; e_dns=$2; e_sai=$3; e_mes=$4; e_out=$5
  r_dns=$(probe a1 10.89.0.1 53); r_sai=$(probe a1 192.0.2.2 443); r_mes=$(probe a1 10.89.0.3 6379); r_out=$(probe a1 10.89.1.2 5432)
  if [ "$r_dns/$r_sai/$r_mes/$r_out" = "$e_dns/$e_sai/$e_mes/$e_out" ]; then
      echo "PASS  $fase   [dns=$r_dns saida=$r_sai mesma-rede=$r_mes outro-projeto=$r_out]"
  else
      echo "FAIL  $fase   obtido dns=$r_dns saida=$r_sai mesma-rede=$r_mes outro-projeto=$r_out | esperado dns=$e_dns saida=$e_sai mesma-rede=$e_mes outro-projeto=$e_out"
      FALHAS=$((FALHAS + 1))
  fi
}
regras() { ufw status numbered | grep -c "rustploy:"; }

confere "P0 sem firewall (o laboratório funciona)"                       OK   OK   OK OK
ufw --force reset >/dev/null 2>&1
ufw default deny incoming >/dev/null; ufw default deny routed >/dev/null; ufw default allow outgoing >/dev/null
ufw --force enable >/dev/null 2>&1 || { echo "FALHA: ufw enable"; exit 2; }
confere "P1 ufw padrão, sem regras (o sintoma do servidor real)"          BLOQ BLOQ OK BLOQ
sh /helper apply >/dev/null
confere "P2 depois do apply"                                               OK   OK   OK BLOQ
ufw route delete deny from 10.89.0.0/16 to 10.89.0.0/16 >/dev/null 2>&1
confere "P3 controle negativo: sem o deny, o outro projeto passa"         OK   OK   OK OK
sh /helper apply >/dev/null
confere "P4 apply restaura o deny (no TOPO)"                               OK   OK   OK BLOQ
sh /helper remove >/dev/null
ufw route allow from 10.89.0.0/16 comment "rustploy: saida dos containers para a internet" >/dev/null 2>&1
ufw route deny from 10.89.0.0/16 to 10.89.0.0/16 comment "rustploy: isola as redes dos containers entre si" >/dev/null 2>&1
confere "P5a ordem errada (só regras de rota, allow antes do deny): isolamento perdido"  BLOQ OK   OK OK
sh /helper apply >/dev/null
confere "P5b apply conserta a ordem"                                       OK   OK   OK BLOQ
a1=$(ufw status numbered | grep "rustploy:"); sh /helper apply >/dev/null; sh /helper apply >/dev/null; a2=$(ufw status numbered | grep "rustploy:")
if [ "$a1" = "$a2" ]; then echo "PASS  P6 dois applies seguidos não mudam nenhuma regra ($(regras) regras)"; else echo "FAIL  P6 regras mudaram"; FALHAS=$((FALHAS + 1)); fi
sh /helper remove >/dev/null
if [ "$(regras)" = "0" ]; then echo "PASS  P7 remove apaga todas as regras do rustploy"; else echo "FAIL  P7 sobraram $(regras) regras"; FALHAS=$((FALHAS + 1)); fi
confere "P8 depois do remove: de volta ao sintoma"                        BLOQ BLOQ OK BLOQ

echo; if [ "$FALHAS" = "0" ]; then echo "todos os cenários passaram"; else echo "$FALHAS cenário(s) FALHARAM"; exit 1; fi
