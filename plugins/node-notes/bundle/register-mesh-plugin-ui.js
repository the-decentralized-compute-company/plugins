/**
 * Console bundle for `node-notes`.
 *
 * Ships as-is: the host imports it as a browser ES module and does not
 * transpile TypeScript, JSX, CommonJS, or bare npm specifiers. It uses only DOM
 * APIs and the host object described in `host-contract.d.ts`.
 *
 * Two rules govern this file.
 *
 * **Note text is never HTML.** Every string that came from a note — its body,
 * author, tags, subject, and the peer id it arrived under — reaches the page
 * through `textContent` or `createTextNode`, never `innerHTML`. A note written
 * on another machine is untrusted input, and this page is the one place it is
 * rendered for a human.
 *
 * **Provenance is visible before the text is.** A note from a peer is drawn
 * with its own badge and border, and the page header says how many of the notes
 * on screen came from machines this node does not control. The API says the
 * same thing in `origin`, `untrusted`, and `trust`; the page just refuses to
 * let it be missed.
 *
 * The page is read-only. Writing, sharing, and expiring are MCP tools, and the
 * plugin exposes no HTTP route that could change anything.
 *
 * @type {import('./host-contract').MeshPluginUiBundleModule}
 */
const moduleRegistration = {
  async registerMeshPluginUi(host) {
    host.state.update({ loadedAt: Date.now() });
    return { pages: { notes: mountNotesPage } };
  },
};
export const registerMeshPluginUi = moduleRegistration.registerMeshPluginUi;

const NOTE_LIMIT = 100;
const REFRESH_MS = 30_000;

const KINDS = ["incident", "change", "pin", "question", "info"];
const ORIGINS = [
  ["any", "All notes"],
  ["local", "This node"],
  ["peer", "From peers"],
];

/* ---------------------------------------------------------------- elements */

function panel(host) {
  const { tokens } = host.appearance;
  const element = document.createElement("section");
  Object.assign(element.style, {
    background: tokens.panel,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radiusLarge,
    color: tokens.foreground,
    display: "grid",
    gap: "12px",
    padding: "16px",
  });
  return element;
}

function heading(host, text, level = "h3") {
  const element = document.createElement(level);
  element.textContent = text;
  Object.assign(element.style, {
    color: host.appearance.tokens.foreground,
    fontSize: level === "h2" ? "20px" : "15px",
    fontWeight: "600",
    margin: "0",
  });
  return element;
}

function paragraph(host, text, tone = "muted") {
  const { tokens } = host.appearance;
  const element = document.createElement("p");
  element.textContent = text;
  Object.assign(element.style, {
    color: tone === "muted" ? tokens.borderSoft : tokens[tone] || tokens.foreground,
    fontSize: "13px",
    lineHeight: "1.5",
    margin: "0",
  });
  return element;
}

function badge(host, text, tone) {
  const { tokens } = host.appearance;
  const colour = tokens[tone] || tokens.borderSoft;
  const element = document.createElement("span");
  element.textContent = text;
  Object.assign(element.style, {
    border: `1px solid ${colour}`,
    borderRadius: "999px",
    color: colour,
    fontSize: "11px",
    fontWeight: "600",
    letterSpacing: "0.02em",
    padding: "2px 8px",
    textTransform: "uppercase",
    whiteSpace: "nowrap",
  });
  return element;
}

function button(host, label) {
  const { tokens } = host.appearance;
  const element = document.createElement("button");
  element.type = "button";
  element.textContent = label;
  Object.assign(element.style, {
    background: tokens.panelStrong,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radius,
    color: tokens.foreground,
    cursor: "pointer",
    font: "inherit",
    fontWeight: "600",
    minHeight: "34px",
    padding: "0 12px",
  });
  return element;
}

function select(host, options, value) {
  const { tokens } = host.appearance;
  const element = document.createElement("select");
  for (const [optionValue, optionLabel] of options) {
    const option = document.createElement("option");
    option.value = optionValue;
    option.textContent = optionLabel;
    element.append(option);
  }
  element.value = value;
  Object.assign(element.style, {
    background: tokens.panelStrong,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radius,
    color: tokens.foreground,
    font: "inherit",
    minHeight: "34px",
    padding: "0 8px",
  });
  return element;
}

function textInput(host, placeholder) {
  const { tokens } = host.appearance;
  const element = document.createElement("input");
  element.type = "search";
  element.placeholder = placeholder;
  Object.assign(element.style, {
    background: tokens.panelStrong,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radius,
    color: tokens.foreground,
    font: "inherit",
    minHeight: "34px",
    minWidth: "200px",
    padding: "0 10px",
  });
  return element;
}

/* ----------------------------------------------------------------- helpers */

function relativeAge(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return "expired";
  if (seconds < 90) return `${Math.round(seconds)}s left`;
  if (seconds < 5_400) return `${Math.round(seconds / 60)}m left`;
  if (seconds < 172_800) return `${Math.round(seconds / 3_600)}h left`;
  return `${Math.round(seconds / 86_400)}d left`;
}

function shortPeer(peerId) {
  if (typeof peerId !== "string" || peerId.length <= 14) return peerId || "";
  return `${peerId.slice(0, 8)}…${peerId.slice(-4)}`;
}

function toneForKind(kind) {
  if (kind === "incident") return "bad";
  if (kind === "question") return "warn";
  if (kind === "change" || kind === "pin") return "accent";
  return "borderSoft";
}

/** Build the plugin-relative query the host will fetch for us. */
function notesPath(query, filters) {
  const parameters = new URLSearchParams();
  parameters.set("limit", String(NOTE_LIMIT));
  if (filters.origin && filters.origin !== "any") parameters.set("origin", filters.origin);
  if (filters.kind) parameters.set("kind", filters.kind);
  const trimmed = query.trim();
  if (trimmed) {
    parameters.set("query", trimmed);
    return `http/search?${parameters.toString()}`;
  }
  return `http/notes?${parameters.toString()}`;
}

/* -------------------------------------------------------------- note cards */

function noteCard(host, note) {
  const { tokens } = host.appearance;
  const fromPeer = note.origin === "peer";
  const card = document.createElement("article");
  Object.assign(card.style, {
    background: tokens.panelStrong,
    border: `1px solid ${fromPeer ? tokens.warn : tokens.border}`,
    borderLeft: `3px solid ${fromPeer ? tokens.warn : tokens.accent}`,
    borderRadius: tokens.radius,
    display: "grid",
    gap: "8px",
    padding: "12px",
  });

  const top = document.createElement("div");
  Object.assign(top.style, {
    alignItems: "center",
    display: "flex",
    flexWrap: "wrap",
    gap: "6px",
  });
  top.append(badge(host, note.kind, toneForKind(note.kind)));
  top.append(badge(host, note.subject, "borderSoft"));
  if (fromPeer) {
    // Deliberately the loudest thing on the card.
    top.append(badge(host, `from ${shortPeer(note.from_peer)}`, "warn"));
  } else {
    top.append(badge(host, note.shared ? "local · shared" : "local", "accent"));
  }
  const age = document.createElement("span");
  age.textContent = relativeAge(note.expires_in_secs);
  Object.assign(age.style, {
    color: tokens.borderSoft,
    fontSize: "12px",
    marginLeft: "auto",
  });
  top.append(age);
  card.append(top);

  // `textContent`, never `innerHTML`: this string was written elsewhere.
  const body = document.createElement("p");
  body.textContent = note.text;
  Object.assign(body.style, {
    color: tokens.foreground,
    fontSize: "14px",
    lineHeight: "1.5",
    margin: "0",
    whiteSpace: "pre-wrap",
    wordBreak: "break-word",
  });
  card.append(body);

  if (fromPeer) {
    card.append(paragraph(host, note.trust, "warn"));
  }

  const footer = document.createElement("div");
  Object.assign(footer.style, {
    alignItems: "center",
    color: tokens.borderSoft,
    display: "flex",
    flexWrap: "wrap",
    fontSize: "12px",
    gap: "8px",
  });
  if (note.author) {
    const author = document.createElement("span");
    author.textContent = `— ${note.author}`;
    footer.append(author);
  }
  for (const tag of note.tags || []) {
    footer.append(badge(host, `#${tag}`, "borderSoft"));
  }
  const identifier = document.createElement("code");
  identifier.textContent = note.id;
  Object.assign(identifier.style, { fontSize: "11px", marginLeft: "auto" });
  footer.append(identifier);
  card.append(footer);

  return card;
}

/* ------------------------------------------------------------------- page */

function mountNotesPage({ element, host }) {
  const { tokens } = host.appearance;
  const filters = { origin: "any", kind: "" };
  let query = "";
  let disposed = false;

  const root = document.createElement("div");
  Object.assign(root.style, { display: "grid", gap: "16px" });

  const header = panel(host);
  header.append(heading(host, "Notes", "h2"));
  const summary = paragraph(
    host,
    "Short-lived operational notes about this node and the mesh. Read-only here: notes are written, shared, and expired through this plugin's MCP tools.",
  );
  header.append(summary);
  const sharingLine = paragraph(host, "Loading…");
  header.append(sharingLine);

  const controls = document.createElement("div");
  Object.assign(controls.style, {
    alignItems: "center",
    display: "flex",
    flexWrap: "wrap",
    gap: "8px",
  });
  const search = textInput(host, "Search every term…");
  const originSelect = select(host, ORIGINS, filters.origin);
  const kindSelect = select(
    host,
    [["", "Any kind"]].concat(KINDS.map((kind) => [kind, kind])),
    filters.kind,
  );
  const refresh = button(host, "Refresh");
  controls.append(search, originSelect, kindSelect, refresh);
  header.append(controls);
  root.append(header);

  const listPanel = panel(host);
  const listHeading = heading(host, "Loading notes…");
  const provenanceLine = paragraph(host, "");
  const list = document.createElement("div");
  Object.assign(list.style, { display: "grid", gap: "10px" });
  listPanel.append(listHeading, provenanceLine, list);
  root.append(listPanel);

  element.replaceChildren(root);

  function renderError(where, error) {
    list.replaceChildren();
    listHeading.textContent = `Could not load ${where}`;
    const message = document.createElement("p");
    message.textContent = String(error && error.message ? error.message : error);
    Object.assign(message.style, { color: tokens.bad, fontSize: "13px", margin: "0" });
    list.append(message);
  }

  async function loadStatus() {
    try {
      const status = await host.network.json("http/status");
      if (disposed) return;
      const sharing = status.sharing || {};
      sharingLine.textContent = sharing.enabled
        ? `Sharing is on. ${sharing.published} published, ${sharing.received} received from ${(status.peers || []).length} peers. ${sharing.reach}.`
        : `Sharing is off — nothing leaves this node and nothing inbound is kept. ${sharing.reason || ""}`;
      sharingLine.style.color = sharing.enabled ? tokens.good : tokens.borderSoft;
    } catch (error) {
      if (disposed) return;
      sharingLine.textContent = `Status unavailable: ${error && error.message ? error.message : error}`;
      sharingLine.style.color = tokens.bad;
    }
  }

  async function loadNotes() {
    try {
      const payload = await host.network.json(notesPath(query, filters));
      if (disposed) return;
      const notes = Array.isArray(payload.notes) ? payload.notes : [];
      listHeading.textContent = notes.length
        ? `${payload.returned} of ${payload.matched} notes`
        : "No notes match";
      const untrusted = notes.filter((note) => note.untrusted).length;
      provenanceLine.textContent = untrusted
        ? `${untrusted} of these were written on other machines. ${payload.disclaimer}`
        : "Every note here was written on this node.";
      provenanceLine.style.color = untrusted ? tokens.warn : tokens.borderSoft;
      list.replaceChildren(...notes.map((note) => noteCard(host, note)));
      if (!notes.length) {
        list.append(
          paragraph(
            host,
            query.trim()
              ? "Nothing matches every word of that query."
              : "Nothing has been written here yet, or everything written has expired.",
          ),
        );
      }
    } catch (error) {
      if (disposed) return;
      renderError("notes", error);
    }
  }

  function reload() {
    void loadStatus();
    void loadNotes();
  }

  let debounce = 0;
  search.addEventListener("input", () => {
    query = search.value;
    window.clearTimeout(debounce);
    debounce = window.setTimeout(() => void loadNotes(), 250);
  });
  originSelect.addEventListener("change", () => {
    filters.origin = originSelect.value;
    void loadNotes();
  });
  kindSelect.addEventListener("change", () => {
    filters.kind = kindSelect.value;
    void loadNotes();
  });
  refresh.addEventListener("click", reload);

  reload();
  const timer = window.setInterval(reload, REFRESH_MS);

  return {
    unmount() {
      disposed = true;
      window.clearTimeout(debounce);
      window.clearInterval(timer);
      element.replaceChildren();
    },
  };
}
