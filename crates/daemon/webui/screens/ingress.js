// screens/ingress.js — tela "Ingress": rotas ativas no reverse proxy (por
// domínio) e portas TCP de host expostas diretamente. Porta da seção
// `equals="ingress"` de home.gv. Também as **rotas próprias** (domínio fora do
// serviço: caminhos para vários serviços, redirect, senha) e o editor delas —
// ver docs/arquivo/plano-releases-ambientes-rotas-volumes.md §3.
import { servicePairList, ingressRows, hostPortRows } from "../format.js";
import { t } from "../i18n.js";

function emptyRule() {
  return { path_prefix: "/", service_id: "", port: "", strip_prefix: false };
}

document.addEventListener("alpine:init", () => {
  Alpine.data("ingress", () => ({
    get store() {
      return Alpine.store("app");
    },
    customRoutes: [], // RouteList
    routeEditor: null, // a rota sendo criada/editada; null = editor fechado
    routeError: "",

    init() {
      this.$watch(
        () => this.store.view,
        (view) => {
          if (view === "ingress") this.loadCustomRoutes();
        }
      );
      if (this.store.view === "ingress") this.loadCustomRoutes();
    },

    get pairs() {
      return servicePairList(this.store.snap?.services);
    },
    get routes() {
      return ingressRows(this.pairs);
    },
    get hostPorts() {
      return hostPortRows(this.pairs);
    },

    get projects() {
      return (this.store.snap && this.store.snap.projects) || [];
    },
    serviceName(serviceId) {
      const pair = this.pairs.find((item) => item.svc.id === serviceId);
      return pair ? pair.svc.spec.name : serviceId;
    },
    servicesOfProject(projectId) {
      return this.pairs
        .filter((pair) => pair.svc.spec.project_id === projectId)
        .map((pair) => ({ id: pair.svc.id, name: pair.svc.spec.name }));
    },

    get customRouteRows() {
      return this.customRoutes.map((route) => {
        let target;
        if (route.action.Redirect) {
          target = "↪ " + route.action.Redirect.to + (route.action.Redirect.permanent ? " (301)" : " (302)");
        } else {
          target = (route.action.Forward?.rules || [])
            .map((rule) => rule.path_prefix + " → " + this.serviceName(rule.service_id)
              + (rule.port ? ":" + rule.port : "") + (rule.strip_prefix ? t(" (sem prefixo)") : ""))
            .join(" · ");
        }
        return {
          id: route.id,
          domain: route.domain,
          target,
          tls: route.tls ? "✓" : "—",
          auth: route.basic_auth ? t("senha") : "",
          route,
        };
      });
    },

    async loadCustomRoutes() {
      const routeListResponse = await this.store.api.rpc("RouteList");
      this.customRoutes = (routeListResponse.ok && routeListResponse.value?.Routes) || [];
      return null;
    },

    openRouteEditor(route) {
      this.routeError = "";
      if (!route) {
        const firstProject = this.projects[0];
        this.routeEditor = {
          id: "",
          project_id: firstProject ? firstProject.id : "",
          domain: "",
          tls: true,
          mode: "forward",
          rules: [emptyRule()],
          to: "",
          permanent: true,
          auth_enabled: false,
          auth_user: "",
          auth_password: "",
          had_password: false,
        };
        return;
      }
      const forward = route.action.Forward;
      const redirect = route.action.Redirect;
      this.routeEditor = {
        id: route.id,
        project_id: route.project_id,
        domain: route.domain,
        tls: route.tls,
        mode: redirect ? "redirect" : "forward",
        rules: forward
          ? forward.rules.map((rule) => ({ ...rule, port: rule.port ? String(rule.port) : "" }))
          : [emptyRule()],
        to: redirect ? redirect.to : "",
        permanent: redirect ? redirect.permanent : true,
        auth_enabled: !!route.basic_auth,
        auth_user: route.basic_auth ? route.basic_auth.user : "",
        auth_password: "",
        had_password: !!route.basic_auth,
      };
    },
    closeRouteEditor() {
      this.routeEditor = null;
    },
    addRouteRule() {
      this.routeEditor.rules.push({ ...emptyRule(), path_prefix: "" });
    },
    removeRouteRule(index) {
      this.routeEditor.rules.splice(index, 1);
    },

    async saveRoute() {
      const editor = this.routeEditor;
      if (!editor) return;
      const action = editor.mode === "redirect"
        ? { Redirect: { to: editor.to.trim(), permanent: !!editor.permanent } }
        : {
          Forward: {
            rules: editor.rules.map((rule) => ({
              path_prefix: rule.path_prefix.trim(),
              service_id: rule.service_id,
              port: rule.port === "" ? null : Number(rule.port),
              strip_prefix: !!rule.strip_prefix,
            })),
          },
        };
      const route = {
        id: editor.id,
        project_id: editor.project_id,
        domain: editor.domain.trim(),
        tls: !!editor.tls,
        action,
        basic_auth: editor.auth_enabled
          ? { user: editor.auth_user.trim(), password: editor.auth_password === "" ? null : editor.auth_password }
          : null,
      };
      const routeSaveResponse = await this.store.api.rpcChecked({ RouteSave: { route } });
      if (routeSaveResponse.ok) {
        this.store.toastOk(t`rota ${route.domain} salva`);
        this.routeEditor = null;
        await this.loadCustomRoutes();
      } else {
        this.routeError = routeSaveResponse.error;
      }
      return routeSaveResponse;
    },

    async deleteRoute(routeId, domain) {
      if (!confirm(t`Apagar a rota de ${domain}?` + "\n\n" + t("O domínio passa a responder 404 até outra rota ou serviço assumi-lo."))) return;
      const routeDeleteResponse = await this.store.api.rpcChecked({ RouteDelete: { route_id: routeId } });
      this.store.toastResult(routeDeleteResponse, t`rota ${domain} apagada`);
      if (routeDeleteResponse.ok) await this.loadCustomRoutes();
      return routeDeleteResponse;
    },
  }));
});
