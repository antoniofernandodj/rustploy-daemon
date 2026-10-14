// screens/login.js — tela de login. A maior parte do estado (chave/
// remember/erro) já mora no Alpine.store('app') (app.js) porque a topbar e o
// fluxo de reconexão também precisam dele; este componente só encapsula o
// submit do formulário e o aviso de chave de outro servidor.
import { decodeAccessKey } from "../net/api.js";

document.addEventListener("alpine:init", () => {
  Alpine.data("login", () => ({
    get store() {
      return Alpine.store("app");
    },

    /** Aviso (não bloqueio) quando a chave aponta para outro host: atrás de
     * proxy o host visto pelo navegador pode legitimamente diferir. */
    get otherServerWarning() {
      const decodedKey = decodeAccessKey(this.store.accessKey);
      if (!decodedKey) return "";
      let keyHost;
      try {
        keyHost = new URL(decodedKey.url).host;
      } catch {
        return "";
      }
      return keyHost === window.location.host ? "" : "esta chave é de " + keyHost + " — você está em " + window.location.host;
    },
    submit() {
      this.store.connect();
    },
  }));
});
