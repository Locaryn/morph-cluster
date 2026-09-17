/* morph-cluster — Réglages → Compte → Partage de ressources.
 *
 * Deux écrans dans un seul panneau, selon ce qu'est la machine :
 *
 * - **Poste client** (connecté à un serveur Locaryn) : la case « Allouer cette
 *   machine au partage de ressources », ce qui est prêté (carte, mémoire vive,
 *   stockage et son quota), puis la liste des modèles partagés avec leur état
 *   ici — copié, copie en cours, en attente, place insuffisante.
 * - **Serveur** : les modèles de sa bibliothèque, un interrupteur par modèle
 *   pour le partager, et pour chacun les machines qui en ont déjà une copie.
 *
 * Un poste client peut aussi activer ou couper le partage d'un modèle : la
 * demande part au serveur par le compte de la personne (`server.invokeTool`),
 * jamais par un canal que ce panneau ouvrirait lui-même. Le secret du cluster
 * transite de la même façon, et n'est jamais affiché.
 *
 * Rendu dans le document, sans racine fantôme : le panneau hérite du thème et
 * des classes de l'application. */
(function () {
  "use strict";

  var TAG = "locaryn-cluster-sharing";
  var STYLE_ID = "locaryn-cluster-sharing-style";

  function bridge() {
    return window.locaryn || window.LocarynPluginAPI || null;
  }

  function parse(value) {
    if (typeof value !== "string") return value;
    try {
      return JSON.parse(value);
    } catch (e) {
      return { text: value };
    }
  }

  function erreur(e) {
    return String((e && e.message) || e).replace(/^Error:\s*/, "");
  }

  function localTool(name, args) {
    var api = bridge();
    if (!api || !api.tools) return Promise.reject(new Error("Pont d'outils indisponible."));
    return Promise.resolve(api.tools.invoke(name, args || {})).then(parse);
  }

  /** Un outil du morph sur le serveur, par le compte de la personne. Sur le
   *  serveur lui-même, l'hôte répond localement. */
  function serverTool(name, args) {
    var api = bridge();
    if (!api || !api.server || !api.server.invokeTool) {
      return Promise.reject(
        new Error("Cette version de Locaryn ne sait pas encore joindre le serveur depuis un morph."),
      );
    }
    return Promise.resolve(api.server.invokeTool(name, args || {})).then(parse);
  }

  function go(bytes) {
    var g = (bytes || 0) / 1073741824;
    return (g >= 10 ? Math.round(g) : g.toFixed(1)) + " Go";
  }

  function hostOf(url) {
    try {
      return new URL(url).hostname;
    } catch (e) {
      return null;
    }
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  function nomCourt(file) {
    var parts = String(file).split("/");
    return parts[parts.length - 1].replace(/\.gguf$/i, "");
  }

  function injectStyle() {
    if (document.getElementById(STYLE_ID)) return;
    var s = document.createElement("style");
    s.id = STYLE_ID;
    s.textContent = [
      TAG + "{display:block;max-width:760px}",
      TAG + " .cs-section{margin-bottom:var(--space-6,24px)}",
      TAG + " .cs-head{display:flex;align-items:center;justify-content:space-between;gap:var(--space-3,12px);flex-wrap:wrap;margin-bottom:var(--space-2,8px)}",
      TAG + " .cs-title{font-size:var(--text-md,15px);font-weight:600;color:var(--text)}",
      TAG + " .cs-sub{font-size:var(--text-sm,13px);color:var(--text-dim);margin:0 0 var(--space-3,12px)}",
      TAG + " .cs-state{font-size:var(--text-xs,12px);color:var(--text-dim);border:1px solid var(--border);border-radius:999px;padding:2px 10px;white-space:nowrap}",
      TAG + " .cs-state.is-on{color:var(--accent);border-color:var(--accent)}",
      TAG + " .cs-state.is-warn{color:var(--warn);border-color:var(--warn)}",
      TAG + " .cs-row{display:flex;align-items:center;gap:var(--space-3,12px);min-height:48px;padding:var(--space-2,8px) 0;border-top:1px solid var(--border)}",
      TAG + " .cs-row:first-child{border-top:0}",
      TAG + " .cs-grow{flex:1;min-width:0}",
      TAG + " .cs-label{color:var(--text);font-size:var(--text-sm,13px);overflow-wrap:anywhere}",
      TAG + " .cs-hint{color:var(--text-faint);font-size:var(--text-xs,12px);margin-top:2px;overflow-wrap:anywhere}",
      TAG + " .cs-switch{position:relative;flex:none;width:44px;height:26px;border-radius:999px;border:1px solid var(--border-strong);background:var(--surface-2);cursor:pointer;transition:background var(--dur-fast,120ms) var(--ease,ease),border-color var(--dur-fast,120ms) var(--ease,ease)}",
      TAG + " .cs-switch::after{content:'';position:absolute;top:3px;left:3px;width:18px;height:18px;border-radius:50%;background:var(--text-dim);transition:transform var(--dur-fast,120ms) var(--ease,ease),background var(--dur-fast,120ms) var(--ease,ease)}",
      TAG + " .cs-switch[aria-checked='true']{background:var(--accent-fill,var(--accent));border-color:var(--accent)}",
      TAG + " .cs-switch[aria-checked='true']::after{transform:translateX(18px);background:var(--accent)}",
      TAG + " .cs-switch:focus-visible{outline:2px solid var(--accent);outline-offset:2px}",
      TAG + " .cs-switch:disabled{opacity:.5;cursor:default}",
      TAG + " .cs-main{padding:var(--space-4,16px);border:1px solid var(--border);border-radius:var(--radius,10px);background:var(--surface)}",
      TAG + " .cs-main .cs-row{border-top:0;min-height:44px}",
      TAG + " .cs-details{margin-top:var(--space-3,12px);padding-top:var(--space-2,8px);border-top:1px solid var(--border)}",
      TAG + " .cs-quota{display:flex;align-items:center;gap:var(--space-2,8px)}",
      TAG + " .cs-quota input{width:88px;min-height:36px}",
      TAG + " .cs-bar{height:4px;border-radius:2px;background:var(--surface-2);overflow:hidden;margin-top:6px}",
      TAG + " .cs-bar>span{display:block;height:100%;background:var(--accent);transition:width var(--dur-med,240ms) var(--ease,ease)}",
      TAG + " .cs-list{border:1px solid var(--border);border-radius:var(--radius,10px);padding:0 var(--space-4,16px);background:var(--surface)}",
      TAG + " .cs-tag{font-size:var(--text-xs,12px);color:var(--text-dim);white-space:nowrap}",
      TAG + " .cs-tag.ok{color:var(--accent)}",
      TAG + " .cs-tag.warn{color:var(--warn)}",
      TAG + " .cs-msg{font-size:var(--text-sm,13px);margin:var(--space-2,8px) 0;color:var(--text-dim)}",
      TAG + " .cs-msg.err{color:var(--danger)}",
      TAG + " .cs-tools{display:flex;gap:var(--space-2,8px);flex-wrap:wrap;align-items:center}",
      TAG + " .cs-tools .locaryn-input{flex:1;min-width:180px;min-height:40px}",
      TAG + " .cs-actions{display:flex;gap:var(--space-2,8px);flex-wrap:wrap;margin-top:var(--space-3,12px)}",
      TAG + " .cs-actions button{min-height:40px}",
      TAG + " .cs-empty{padding:var(--space-4,16px) 0;color:var(--text-faint);font-size:var(--text-sm,13px)}",
      TAG + " .cs-dash-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(120px,1fr));gap:var(--space-3,12px);margin-bottom:var(--space-4,16px)}",
      TAG + " .cs-dash-tile{padding:var(--space-3,12px);border:1px solid var(--border);border-radius:var(--radius,10px);background:var(--surface)}",
      TAG + " .cs-dash-value{font-size:var(--text-lg,18px);font-weight:700;color:var(--text)}",
      TAG + " .cs-dash-label{font-size:var(--text-xs,12px);color:var(--text-dim);margin-top:2px}",
      TAG + " .cs-member{border-top:1px solid var(--border);padding:var(--space-3,12px) 0}",
      TAG + " .cs-member:first-child{border-top:0}",
      TAG + " .cs-member-stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:var(--space-3,12px);margin-top:var(--space-2,8px)}",
      TAG + " .cs-meter-label{font-size:var(--text-xs,12px);color:var(--text-dim);margin-bottom:3px;white-space:nowrap}",
    ].join("\n");
    document.head.appendChild(s);
  }

  class LocarynClusterSharing extends HTMLElement {
    constructor() {
      super();
      this.conn = null;
      this.share = null;
      this.models = null;
      this.members = [];
      this.sync = null;
      this.modelsError = null;
      this.error = null;
      this.message = null;
      this.busy = false;
      this.pendingModel = null;
      this.query = "";
      this.showLibrary = false;
      this.confirmLeave = false;
      this.enrolled = false;
      this.timer = null;
      this.loading = false;
    }

    connectedCallback() {
      injectStyle();
      this.render();
      this.load();
      var self = this;
      // Une copie de plusieurs gigaoctets avance pendant qu'on regarde : relire
      // l'état toutes les trois secondes suffit à une barre honnête.
      this.timer = window.setInterval(function () {
        if (!self.busy) self.load(true);
      }, 3000);
    }

    disconnectedCallback() {
      if (this.timer) window.clearInterval(this.timer);
      this.timer = null;
    }

    pluginUpdated() {
      this.render();
    }

    isClient() {
      return !!this.conn && this.conn.mode === "client";
    }

    isServer() {
      return !!this.conn && this.conn.mode === "server";
    }

    async load(quiet) {
      // Une lecture de fond peut déjà courir quand une action demande la
      // sienne : l'abandonner laisserait la lecture en cours, partie avant
      // l'action, réafficher l'état d'avant. On la met en file.
      if (this.loading) {
        this.again = true;
        return this.loadingPromise;
      }
      this.loading = true;
      this.loadingPromise = this.read(quiet);
      await this.loadingPromise;
      if (this.again) {
        this.again = false;
        await this.load(true);
      }
    }

    async read(quiet) {
      var api = bridge();
      try {
        this.conn =
          api && api.server && api.server.connection
            ? await api.server.connection()
            : { mode: "local" };
        this.share = await localTool("cluster_share_get");
        if (this.isServer()) {
          if (!this.enrolled) {
            await localTool("cluster_enroll");
            this.enrolled = true;
          }
          this.readModels(await localTool("cluster_shared_models"));
        } else if (this.isClient()) {
          this.sync = await localTool("cluster_sync_status");
          try {
            this.readModels(await serverTool("cluster_shared_models"));
            this.modelsError = null;
          } catch (e) {
            this.modelsError = erreur(e);
          }
        }
        if (!quiet) this.error = null;
      } catch (e) {
        this.error = erreur(e);
      } finally {
        this.loading = false;
        // Un rafraîchissement de fond ne doit pas recréer le champ où la
        // personne est en train d'écrire : sa saisie serait perdue.
        var saisie = document.activeElement;
        var enCours = quiet && saisie && saisie.tagName === "INPUT" && this.contains(saisie);
        if (!enCours) this.render();
      }
    }

    readModels(view) {
      this.models = (view && view.models) || [];
      this.members = (view && view.members) || [];
    }

    prefs() {
      return (this.share && this.share.prefs) || {
        enabled: false,
        gpu: true,
        ram: false,
        storage: true,
        storage_gb: 64,
      };
    }

    async run(action, success) {
      this.busy = true;
      this.error = null;
      this.message = null;
      this.render();
      try {
        await action();
        if (success) this.message = success;
      } catch (e) {
        this.error = erreur(e);
      } finally {
        this.busy = false;
        this.pendingModel = null;
        await this.load(true);
      }
    }

    /** Cocher la case sur un poste client l'inscrit d'abord auprès du serveur,
     *  par le compte de la personne : aucun code à recopier. */
    toggleSharing(enabled) {
      var self = this;
      return this.run(async function () {
        var p = self.prefs();
        if (enabled && self.isClient() && !p.coordinator_host) {
          var inscription = await serverTool("cluster_enroll");
          var code = inscription && inscription.code_de_pairage;
          if (!code) throw new Error("Le serveur n'a pas renvoyé de code d'inscription.");
          await localTool("cluster_join", {
            pairing_code: code,
            coordinator: hostOf(self.conn.server_url),
          });
        }
        self.share = await localTool("cluster_share_set", {
          enabled: enabled,
          gpu: p.gpu,
          ram: p.ram,
          storage: p.storage,
          storage_gb: p.storage_gb,
        });
      }, enabled ? "Cette machine est allouée au partage." : "Cette machine ne prête plus rien.");
    }

    setPref(key, value) {
      var self = this;
      return this.run(async function () {
        var p = self.prefs();
        var args = {
          enabled: p.enabled,
          gpu: p.gpu,
          ram: p.ram,
          storage: p.storage,
          storage_gb: p.storage_gb,
        };
        args[key] = value;
        self.share = await localTool("cluster_share_set", args);
      });
    }

    toggleModel(file, enabled) {
      var self = this;
      this.pendingModel = file;
      return this.run(async function () {
        var call = self.isClient() ? serverTool : localTool;
        self.readModels(await call("cluster_share_model", { file: file, enabled: enabled }));
      }, enabled
        ? nomCourt(file) + " est partagé : les machines qui hébergent des modèles en font une copie."
        : nomCourt(file) + " n'est plus partagé : les copies des autres machines seront retirées.");
    }

    leave() {
      var self = this;
      if (!this.confirmLeave) {
        this.confirmLeave = true;
        this.render();
        return;
      }
      this.confirmLeave = false;
      return this.run(async function () {
        await localTool("cluster_leave");
      }, "Cette machine a quitté le cluster ; ses copies de modèles partagés sont supprimées.");
    }

    // ── Rendu ────────────────────────────────────────────────────────────

    render() {
      this.textContent = "";
      if (!this.conn || !this.share) {
        this.appendChild(el("p", "cs-msg" + (this.error ? " err" : ""), this.error || "Lecture de l'état du partage…"));
        return;
      }
      if (this.error) this.appendChild(el("p", "cs-msg err", this.error));
      if (this.message) this.appendChild(el("p", "cs-msg", this.message));

      if (this.conn.mode === "local") {
        this.appendChild(this.renderLocal());
        return;
      }
      if (this.isClient()) this.appendChild(this.renderMachine());
      this.appendChild(this.renderModels());
      // Le tableau de bord vient du coordinateur (`shared_models_view`), qui
      // répond aussi bien à un appel local qu'à un appel relayé depuis un
      // poste client (`server.invokeTool`) — pas de raison de le réserver
      // au serveur.
      this.appendChild(this.renderMembers());
    }

    renderLocal() {
      var section = el("section", "cs-section");
      section.appendChild(el("div", "cs-title", "Aucun serveur"));
      section.appendChild(
        el(
          "p",
          "cs-sub",
          "Le partage relie des postes à un serveur Locaryn. Sur la machine qui servira de serveur : Réglages → Serveur & fonctions → Serveur actif. Sur les autres : connectez-vous à ce serveur avec votre compte, puis revenez ici.",
        ),
      );
      return section;
    }

    switchButton(checked, label, onChange, disabled) {
      var b = el("button", "cs-switch");
      b.type = "button";
      b.setAttribute("role", "switch");
      b.setAttribute("aria-checked", checked ? "true" : "false");
      b.setAttribute("aria-label", label);
      b.disabled = !!disabled;
      b.addEventListener("click", function () {
        onChange(!checked);
      });
      return b;
    }

    row(label, hint, control) {
      var r = el("div", "cs-row");
      var t = el("div", "cs-grow");
      t.appendChild(el("div", "cs-label", label));
      if (hint) t.appendChild(el("div", "cs-hint", hint));
      r.appendChild(t);
      if (control) r.appendChild(control);
      return r;
    }

    renderMachine() {
      var self = this;
      var p = this.prefs();
      var devices = this.share.devices || [];
      var cartes = devices.filter(function (d) {
        return !/^cpu/i.test(d.name);
      });
      var cpu = devices.filter(function (d) {
        return /^cpu/i.test(d.name);
      })[0];

      var section = el("section", "cs-section");
      var head = el("div", "cs-head");
      head.appendChild(el("div", "cs-title", "Cette machine"));
      var etat = el("span", "cs-state");
      if (p.enabled && this.share.worker_active) {
        etat.textContent = "Prête au cluster";
        etat.classList.add("is-on");
      } else if (p.enabled && this.share.problem) {
        etat.textContent = "Rien n'est prêté";
        etat.classList.add("is-warn");
      } else {
        etat.textContent = p.enabled ? "Stockage seulement" : "Non partagée";
        if (p.enabled) etat.classList.add("is-on");
      }
      head.appendChild(etat);
      section.appendChild(head);
      section.appendChild(
        el(
          "p",
          "cs-sub",
          "Connectée à " + (this.conn.server_url || "un serveur") + (this.conn.username ? " en tant que " + this.conn.username : "") + ".",
        ),
      );

      var main = el("div", "cs-main");
      main.appendChild(
        this.row(
          "Allouer cette machine au partage de ressources",
          "Le serveur pourra y répartir une partie d'un modèle trop gros pour lui seul, et y copier les modèles partagés.",
          this.switchButton(p.enabled, "Allouer cette machine au partage de ressources", function (v) {
            self.toggleSharing(v);
          }, this.busy),
        ),
      );

      if (p.enabled) {
        var details = el("div", "cs-details");
        var carteHint = cartes.length
          ? cartes
              .map(function (d) {
                return d.description + " — " + (d.free_mib / 1024).toFixed(1) + " Go libres";
              })
              .join(" · ")
          : devices.length
            ? "Aucune carte graphique détectée par llama.cpp."
            : "Détection des appareils en cours…";
        details.appendChild(
          this.row("Carte graphique (VRAM)", carteHint, this.switchButton(p.gpu, "Prêter la carte graphique", function (v) {
            self.setPref("gpu", v);
          }, this.busy)),
        );
        details.appendChild(
          this.row(
            "Mémoire vive (RAM)",
            cpu
              ? (cpu.free_mib / 1024).toFixed(1) + " Go libres — plus lente qu'une carte, mais permet des modèles plus gros."
              : "Pour les couches qu'aucune carte n'accueille.",
            this.switchButton(p.ram, "Prêter la mémoire vive", function (v) {
              self.setPref("ram", v);
            }, this.busy),
          ),
        );

        var quota = el("div", "cs-quota");
        var input = el("input", "locaryn-input");
        input.type = "number";
        input.min = "1";
        input.step = "1";
        input.value = String(Math.round(p.storage_gb));
        input.disabled = !p.storage || this.busy;
        input.setAttribute("aria-label", "Place accordée, en Go");
        input.addEventListener("change", function () {
          var v = Number(input.value);
          if (v > 0) self.setPref("storage_gb", v);
        });
        quota.appendChild(input);
        quota.appendChild(el("span", "cs-tag", "Go"));
        var stockage = this.row(
          "Stockage — héberger les modèles partagés",
          this.sync
            ? go(this.sync.used_bytes) + " utilisés sur " + go(this.sync.quota_bytes) + ". Décocher supprime les copies faites par le cluster ; vos propres modèles ne sont jamais touchés."
            : "Une copie de chaque modèle partagé, dans la limite de la place accordée.",
          this.switchButton(p.storage, "Héberger les modèles partagés", function (v) {
            self.setPref("storage", v);
          }, this.busy),
        );
        details.appendChild(stockage);
        details.appendChild(this.row("Place accordée", null, quota));
        main.appendChild(details);

        if (this.share.problem) main.appendChild(el("p", "cs-msg err", this.share.problem));
        if (this.sync && this.sync.last_error) main.appendChild(el("p", "cs-msg err", this.sync.last_error));
      }
      section.appendChild(main);

      if (p.coordinator_host) {
        var actions = el("div", "cs-actions");
        var quitter = el(
          "button",
          "locaryn-btn-ghost" + (this.confirmLeave ? " locaryn-btn-danger" : ""),
          this.confirmLeave ? "Confirmer : quitter et supprimer les copies" : "Quitter le cluster",
        );
        quitter.type = "button";
        quitter.disabled = this.busy;
        quitter.addEventListener("click", function () {
          self.leave();
        });
        actions.appendChild(quitter);
        section.appendChild(actions);
      }
      return section;
    }

    /** L'état d'un modèle partagé sur ce poste client. */
    localState(file) {
      if (!this.sync || !this.sync.models) return null;
      var entry = this.sync.models.filter(function (m) {
        return m.file === file;
      })[0];
      if (!entry) return { text: "En attente du serveur", kind: "" };
      if (entry.downloading) {
        var pct = entry.size_bytes ? Math.floor((entry.copied_bytes / entry.size_bytes) * 100) : 0;
        return { text: "Copie " + pct + " %", kind: "", progress: pct };
      }
      switch (entry.state) {
        case "ready":
          return { text: "Installé ici", kind: "ok" };
        case "pending":
          return { text: "Copie à venir", kind: "" };
        case "preparing":
          return { text: "Préparation sur le serveur", kind: "" };
        case "no_room":
          return { text: "Place insuffisante (manque " + go(entry.missing_bytes) + ")", kind: "warn" };
        default:
          return { text: "Non hébergé ici", kind: "" };
      }
    }

    renderModels() {
      var self = this;
      var section = el("section", "cs-section");
      var head = el("div", "cs-head");
      head.appendChild(el("div", "cs-title", "Modèles partagés"));
      section.appendChild(head);
      section.appendChild(
        el(
          "p",
          "cs-sub",
          "Un modèle partagé s'installe sur chaque machine qui héberge des modèles, en plus du serveur. Le couper retire ces copies.",
        ),
      );

      if (this.modelsError) {
        section.appendChild(el("p", "cs-msg err", "Liste du serveur indisponible : " + this.modelsError));
        return section;
      }
      var models = this.models || [];
      var partages = models.filter(function (m) {
        return m.shared;
      });
      var autres = models.filter(function (m) {
        return !m.shared;
      });

      var list = el("div", "cs-list");
      if (!partages.length) {
        list.appendChild(el("div", "cs-empty", "Aucun modèle n'est partagé pour l'instant."));
      }
      partages.forEach(function (m) {
        list.appendChild(self.modelRow(m));
      });
      section.appendChild(list);

      if (autres.length) {
        var tools = el("div", "cs-tools");
        tools.style.marginTop = "var(--space-4,16px)";
        var toggle = el(
          "button",
          "locaryn-btn-ghost",
          this.showLibrary ? "Masquer la bibliothèque du serveur" : "Partager un autre modèle (" + autres.length + ")",
        );
        toggle.type = "button";
        toggle.addEventListener("click", function () {
          self.showLibrary = !self.showLibrary;
          self.render();
        });
        tools.appendChild(toggle);
        section.appendChild(tools);

        if (this.showLibrary) {
          var search = el("div", "cs-tools");
          search.style.marginTop = "var(--space-2,8px)";
          var input = el("input", "locaryn-input");
          input.type = "search";
          input.placeholder = "Chercher un modèle";
          input.value = this.query;
          input.addEventListener("input", function () {
            self.query = input.value;
            self.renderLibraryInto(libList, autres);
          });
          search.appendChild(input);
          section.appendChild(search);
          var libList = el("div", "cs-list");
          libList.style.marginTop = "var(--space-2,8px)";
          this.renderLibraryInto(libList, autres);
          section.appendChild(libList);
          // Le rendu recrée le champ : on lui rend le focus pour que la saisie
          // ne s'interrompe pas à chaque rafraîchissement.
          if (this.query) {
            window.requestAnimationFrame(function () {
              input.focus();
              input.setSelectionRange(input.value.length, input.value.length);
            });
          }
        }
      }
      return section;
    }

    renderLibraryInto(container, autres) {
      var self = this;
      container.textContent = "";
      var q = this.query.trim().toLowerCase();
      var visibles = autres.filter(function (m) {
        return !q || m.file.toLowerCase().indexOf(q) !== -1;
      });
      if (!visibles.length) {
        container.appendChild(el("div", "cs-empty", "Aucun modèle ne correspond."));
        return;
      }
      visibles.slice(0, 50).forEach(function (m) {
        container.appendChild(self.modelRow(m));
      });
    }

    modelRow(m) {
      var self = this;
      var r = el("div", "cs-row");
      var t = el("div", "cs-grow");
      t.appendChild(el("div", "cs-label", nomCourt(m.file)));
      var infos = [go(m.size_bytes)];
      if (m.shared) {
        if (!m.ready_to_copy) infos.push("empreinte en calcul sur le serveur");
        infos.push(
          m.hosted_by && m.hosted_by.length
            ? "copié sur " + m.hosted_by.join(", ")
            : "pas encore copié ailleurs",
        );
      }
      t.appendChild(el("div", "cs-hint", infos.join(" · ")));
      if (m.shared && this.isClient()) {
        var local = this.localState(m.file);
        if (local && local.progress !== undefined) {
          var bar = el("div", "cs-bar");
          var fill = el("span");
          fill.style.width = local.progress + "%";
          bar.appendChild(fill);
          t.appendChild(bar);
        }
      }
      r.appendChild(t);
      if (m.shared && this.isClient()) {
        var s = this.localState(m.file);
        if (s) r.appendChild(el("span", "cs-tag " + s.kind, s.text));
      }
      r.appendChild(
        this.switchButton(
          m.shared,
          (m.shared ? "Cesser de partager " : "Partager ") + nomCourt(m.file),
          function (v) {
            self.toggleModel(m.file, v);
          },
          this.busy,
        ),
      );
      return r;
    }

    /** Barre d'occupation étiquetée : "3.2 / 8.0 Go" ou "42 %" sans total. */
    meter(usedLabel, pct) {
      var wrap = el("div");
      var bar = el("div", "cs-bar");
      var fill = el("span");
      fill.style.width = Math.max(0, Math.min(100, pct)) + "%";
      bar.appendChild(fill);
      wrap.appendChild(el("div", "cs-meter-label", usedLabel));
      wrap.appendChild(bar);
      return wrap;
    }

    /** Somme des capacités annoncées par chaque machine — le cluster comme
     *  s'il était une seule machine. Une machine qui n'a encore rien annoncé
     *  (total_ram_gb à zéro) est comptée dans le nombre de postes, pas dans
     *  les moyennes : un zéro non mesuré tirerait la moyenne CPU vers le bas
     *  sans rien dire de vrai. */
    aggregate() {
      var members = this.members || [];
      var withCpu = members.filter(function (m) {
        return m.total_ram_gb > 0;
      });
      var sum = function (key) {
        return members.reduce(function (acc, m) {
          return acc + (m[key] || 0);
        }, 0);
      };
      var totalVram = sum("total_vram_gb");
      var freeVram = sum("free_vram_gb");
      var totalRam = sum("total_ram_gb");
      var freeRam = sum("free_ram_gb");
      var avgCpu = withCpu.length
        ? withCpu.reduce(function (acc, m) {
            return acc + m.cpu_usage_percent;
          }, 0) / withCpu.length
        : 0;
      return {
        machines: members.length,
        totalVram: totalVram,
        usedVram: Math.max(0, totalVram - freeVram),
        totalRam: totalRam,
        usedRam: Math.max(0, totalRam - freeRam),
        avgCpu: avgCpu,
      };
    }

    renderMembers() {
      var section = el("section", "cs-section");
      section.appendChild(el("div", "cs-title", "Machines du cluster"));
      section.appendChild(
        el("p", "cs-sub", "Les postes connectés à ce serveur qui ont rejoint le cluster, ce qu'ils prêtent, et leur charge en direct."),
      );

      if (!this.members.length) {
        var vide = el("div", "cs-list");
        vide.appendChild(
          el(
            "div",
            "cs-empty",
            "Aucun poste pour l'instant. Sur un poste connecté à ce serveur : Réglages → Compte → Partage de ressources.",
          ),
        );
        section.appendChild(vide);
        return section;
      }

      var agg = this.aggregate();
      var dash = el("div", "cs-dash-grid");
      var tile = function (label, value) {
        var t = el("div", "cs-dash-tile");
        t.appendChild(el("div", "cs-dash-value", value));
        t.appendChild(el("div", "cs-dash-label", label));
        return t;
      };
      dash.appendChild(tile("Machines", String(agg.machines)));
      dash.appendChild(
        tile("VRAM du cluster", agg.totalVram > 0 ? go(agg.usedVram * 1073741824) + " / " + go(agg.totalVram * 1073741824) : "—"),
      );
      dash.appendChild(
        tile("RAM du cluster", agg.totalRam > 0 ? go(agg.usedRam * 1073741824) + " / " + go(agg.totalRam * 1073741824) : "—"),
      );
      dash.appendChild(tile("CPU moyen", agg.totalRam > 0 ? Math.round(agg.avgCpu) + " %" : "—"));
      section.appendChild(dash);

      var list = el("div", "cs-list");
      var self = this;
      this.members.forEach(function (m) {
        list.appendChild(self.memberCard(m));
      });
      section.appendChild(list);
      return section;
    }

    memberCard(m) {
      var pretes = [];
      if (m.gpu) pretes.push("carte");
      if (m.ram) pretes.push("mémoire vive");
      if (m.storage) pretes.push("stockage");
      var hint =
        (m.is_self ? "Cette machine" : m.address) +
        " · " +
        (m.sharing
          ? "Prête " + (pretes.join(", ") || "rien") + (m.lendable_gb > 0 ? " · " + m.lendable_gb.toFixed(1) + " Go de calcul" : "")
          : "Membre, ne prête rien");

      var card = el("div", "cs-member");
      var head = el("div", "cs-row");
      head.style.borderTop = "0";
      var t = el("div", "cs-grow");
      var titre = el("div", "cs-label", m.name + (m.gpu_name ? " — " + m.gpu_name : ""));
      t.appendChild(titre);
      t.appendChild(el("div", "cs-hint", hint));
      head.appendChild(t);
      head.appendChild(el("span", "cs-tag" + (m.sharing ? " ok" : ""), m.sharing ? "Partage" : "Inactif"));
      card.appendChild(head);

      var mesure = m.total_ram_gb > 0;
      if (mesure) {
        var stats = el("div", "cs-member-stats");
        if (m.total_vram_gb > 0) {
          var vramUse = Math.max(0, m.total_vram_gb - m.free_vram_gb);
          stats.appendChild(
            this.meter(
              "VRAM · " + vramUse.toFixed(1) + " / " + m.total_vram_gb.toFixed(1) + " Go",
              (vramUse / m.total_vram_gb) * 100,
            ),
          );
        }
        var ramUse = Math.max(0, m.total_ram_gb - m.free_ram_gb);
        stats.appendChild(
          this.meter("RAM · " + ramUse.toFixed(1) + " / " + m.total_ram_gb.toFixed(1) + " Go", (ramUse / m.total_ram_gb) * 100),
        );
        stats.appendChild(this.meter("CPU · " + Math.round(m.cpu_usage_percent) + " %", m.cpu_usage_percent));
        card.appendChild(stats);
      }
      return card;
    }
  }

  if (!customElements.get(TAG)) customElements.define(TAG, LocarynClusterSharing);
})();
