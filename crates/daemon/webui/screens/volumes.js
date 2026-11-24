// screens/volumes.js — tela "Volumes": os volumes que guardam dados, com quem
// monta cada um (serviço e caminho; emprestado = externo num Compose), dono,
// órfão e backup; adotar num serviço Application e apagar. Porta de
// handlers/volumes.luau. Ver docs/plano-volumes-tela-e-emprestados.md.
import { formatBytes } from "../format.js";
import { t } from "../i18n.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("volumesScreen", () => ({
    get store() {
      return Alpine.store("app");
    },
    volumes: [],
    onlyOrphans: false,
    showAnonymous: false,
    adopt: null, // { name, serviceId, target } enquanto o "Adotar" está aberto

    init() {
      this.$watch(() => this.store.view, (view) => {
        if (view === "volumes") this.load();
      });
      if (this.store.view === "volumes") this.load();
    },

    async load() {
      const volumeListResponse = await this.store.api.rpc("VolumeList");
      this.volumes = (volumeListResponse.ok && volumeListResponse.value?.Volumes) || [];
      return null;
    },

    get rows() {
      return this.volumes
        .filter((volume) => this.showAnonymous || !volume.anonymous)
        .filter((volume) => !this.onlyOrphans || volume.orphan_of)
        .map((volume) => {
          const mounts = (volume.mounts || [])
            .map((mount) => mount.service + " (" + mount.project + ") → " + mount.container_path
              + (mount.borrowed ? t(" · emprestado") : ""))
            .join("  ·  ");
          let status = t("sem uso");
          let kind = "muted";
          if (volume.orphan_of) {
            status = t`órfão: ${volume.orphan_of}`;
            kind = "warn";
          } else if (volume.in_use) {
            status = t("em uso");
            kind = "ok";
          }
          return {
            name: volume.name,
            mounts: mounts || t("ninguém monta"),
            size: volume.size_bytes >= 0 ? formatBytes(volume.size_bytes) : "—",
            status,
            kind,
            backup: volume.has_backup_policy ? t("backup ligado") : "",
            canRemove: !volume.in_use,
            // Sem uso de ninguém: adotar um volume em uso poria dois containers nos mesmos dados.
            canAdopt: (volume.mounts || []).length === 0 && !volume.in_use,
          };
        });
    },

    get applicationServices() {
      return ((this.store.snap && this.store.snap.services) || [])
        .filter((pair) => !pair.service.spec.source.Compose)
        .map((pair) => ({ id: pair.service.id, label: pair.service.spec.name + " (" + (pair.project_name || "") + ")" }))
        .sort((left, right) => left.label.localeCompare(right.label));
    },

    openAdopt(name) {
      this.adopt = { name, serviceId: "", target: "" };
    },
    cancelAdopt() {
      this.adopt = null;
    },
    async confirmAdopt() {
      const adopt = this.adopt;
      if (!adopt || !adopt.serviceId || !adopt.target.trim()) {
        this.store.toastError(t("Escolha o serviço e o caminho no container."));
        return;
      }
      const attachResponse = await this.store.api.rpcChecked({
        ServiceVolumeAttach: {
          service_id: adopt.serviceId,
          volume: { host_path: adopt.name, container_path: adopt.target.trim(), read_only: false },
        },
      });
      this.store.toastResult(attachResponse, t`${adopt.name} montado em ${adopt.target.trim()}: vale no próximo deploy`);
      if (attachResponse.ok) {
        this.adopt = null;
        await this.load();
      }
      return attachResponse;
    },

    async remove(name) {
      if (!confirm(t`Apagar o volume ${name}?` + "\n\n" + t("Os dados dele são apagados, sem volta."))) return;
      const removeResponse = await this.store.api.rpcChecked({ RemoveVolume: { name } });
      this.store.toastResult(removeResponse, t`volume ${name} apagado`);
      if (removeResponse.ok) await this.load();
      return removeResponse;
    },
  }));
});
