// screens/login.js — tela de login. A maior parte do estado (chave/
// remember/erro) já mora no Alpine.store('app') (app.js) porque a topbar e o
// fluxo de reconexão também precisam dele; este componente só encapsula o
// submit do formulário e o aviso de token de outro servidor.
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
      // `location.host` omite a porta padrão; o token de conexão sempre a tem.
      const here = window.location.hostname + ":" + (window.location.port || (window.location.protocol === "https:" ? "443" : "80"));
      return decodedKey.address === here ? "" : "este token é de " + decodedKey.address + " — você está em " + here;
    },
    submit() {
      this.store.connect();
    },
  }));
});
