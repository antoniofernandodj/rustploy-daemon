// screens/docker.js — tela "Docker": containers/imagens/volumes/networks do
// host inteiro (não só recursos geridos pelo rustploy) + sub-aba Registry.
// Porta da seção `equals="docker"` de home.gv. Mutações vivem no store
// (app.js); este módulo só formata pra exibição e guarda o estado local do
// modal "novo token de registry" (equivalente ao `new_registry_token_window`
// do cliente iced — aqui vira modal inline, sem motor de janela separado).
import {
  dockerContainerRows,
  dockerImageRows,
  dockerVolumeRows,
  dockerNetworkRows,
  registryRepoRows,
  registryTagRows,
  registryTokenRows,
} from "../format.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("docker", () => ({
    get store() {
      return Alpine.store("app");
    },

    get containers() {
      return dockerContainerRows(this.store.snap?.docker_containers, this.store.search);
    },
    get images() {
      const rows = dockerImageRows(this.store.snap?.docker_images, this.store.search);
      return this.store.onlyUsedImages ? rows.filter((row) => row.inUse) : rows;
    },
    get volumes() {
      const rows = dockerVolumeRows(this.store.snap?.docker_volumes, this.store.search);
      return this.store.onlyUsedVolumes ? rows.filter((row) => row.inUse) : rows;
    },
    get networks() {
      const rows = dockerNetworkRows(this.store.snap?.docker_networks, this.store.search);
      return this.store.onlyUsedNetworks ? rows.filter((row) => row.inUse) : rows;
    },
    get repos() {
      return registryRepoRows(this.store.snap?.registry_repos, this.store.search);
    },
    get tags() {
      return registryTagRows(this.store.registryTags);
    },
    get tokens() {
      return registryTokenRows(this.store.registryTokens);
    },
    get registryHost() {
      const registryStatus = this.store.snap?.registry_status;
      if (!registryStatus) return "127.0.0.1:5100";
      return registryStatus.domain && registryStatus.domain.trim() ? registryStatus.domain : `127.0.0.1:${registryStatus.port}`;
    },
    get registryStatusLabel() {
      const registryStatus = this.store.snap?.registry_status;
      if (!registryStatus) return "desabilitado";
      return registryStatus.enabled ? `ativo em ${this.registryHost}` : "desabilitado";
    },

    // ── Modal "novo token" ───────────────────────────────────────────────
    showTokenModal: false,
    newRegistryTokenStep: "form", // "form" | "reveal"
    newRegistryTokenName: "",
    newRegistryTokenScope: "pull",
    newRegistryTokenError: "",
    newRegistryTokenLoginCommand: "",

    openTokenModal() {
      this.showTokenModal = true;
      this.newRegistryTokenStep = "form";
      this.newRegistryTokenName = "";
      this.newRegistryTokenScope = "pull";
      this.newRegistryTokenError = "";
      this.newRegistryTokenLoginCommand = "";
    },
    closeTokenModal() {
      this.showTokenModal = false;
    },
    async newRegistryTokenCreate() {
      if (!this.newRegistryTokenName.trim()) {
        this.newRegistryTokenError = "nome obrigatório";
        return;
      }
      this.newRegistryTokenError = "";
      const registryCreateTokenResult = await this.store.registryCreateToken(this.newRegistryTokenName.trim(), this.newRegistryTokenScope);
      if (!registryCreateTokenResult.ok) {
        this.newRegistryTokenError = registryCreateTokenResult.error;
        return;
      }
      this.newRegistryTokenLoginCommand = `docker login ${this.registryHost} -u ${this.newRegistryTokenName.trim()} -p ${registryCreateTokenResult.secret}`;
      this.newRegistryTokenStep = "reveal";
    },
  }));
});
