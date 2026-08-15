/**
 * Console bundle for the `notes-console` example.
 *
 * This file ships as-is: the host imports it as a browser ES module and does
 * not transpile TypeScript, JSX, CommonJS, or bare npm specifiers. It uses
 * only DOM APIs and the host object described in `host-contract.d.ts`.
 *
 * @type {import('./host-contract').MeshPluginUiBundleModule}
 */
const moduleRegistration = {
  async registerMeshPluginUi(host) {
    host.state.update({ loadedAt: Date.now() });
    return {
      pages: { notes: mountNotesPage },
      configSections: { "notes-actions": mountNotesActionsSection },
    };
  },
};
export const registerMeshPluginUi = moduleRegistration.registerMeshPluginUi;

const DEFAULT_LIMIT = 25;
const MAX_LIMIT = 200;

/**
 * Read the operator setting the plugin declared in its config schema. Settings
 * live in host-owned config, so the bundle reads them from the host rather
 * than from the plugin process.
 */
function notesPerPage(host) {
  const configured = Number(host.config.visible.settings.max_notes ?? DEFAULT_LIMIT);
  return Number.isFinite(configured)
    ? Math.max(1, Math.min(MAX_LIMIT, Math.round(configured)))
    : DEFAULT_LIMIT;
}

function styleButton(button, host, primary = false) {
  const { tokens } = host.appearance;
  Object.assign(button.style, {
    alignItems: "center",
    background: primary ? tokens.accent : tokens.panelStrong,
    border: `1px solid ${primary ? tokens.accent : tokens.border}`,
    borderRadius: tokens.radius,
    color: primary ? tokens.accentInk : tokens.foreground,
    cursor: "pointer",
    display: "inline-flex",
    font: "inherit",
    fontWeight: "600",
    minHeight: "36px",
    padding: "0 12px",
  });
}

function mountNotesPage({ element, host, page }) {
  const { tokens } = host.appearance;
  const limit = notesPerPage(host);
  // Cancelled on unmount so a late response cannot write into a torn-down DOM.
  let disposed = false;

  Object.assign(element.style, {
    display: "grid",
    gap: "18px",
    maxWidth: "760px",
    padding: "4px",
  });

  const heading = document.createElement("h2");
  heading.textContent = page.label;
  Object.assign(heading.style, {
    color: tokens.foreground,
    fontSize: "1.25rem",
    fontWeight: "650",
    margin: "0",
  });

  const status = document.createElement("p");
  status.setAttribute("aria-live", "polite");
  status.setAttribute("role", "status");
  status.textContent = "Loading notes…";
  Object.assign(status.style, { color: tokens.foreground, margin: "0" });

  const form = document.createElement("form");
  Object.assign(form.style, { display: "flex", flexWrap: "wrap", gap: "10px" });
  const input = document.createElement("input");
  input.type = "text";
  input.required = true;
  input.placeholder = "Add a note";
  Object.assign(input.style, {
    background: tokens.panel,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radius,
    color: tokens.foreground,
    flex: "1 1 260px",
    font: "inherit",
    minHeight: "36px",
    padding: "0 10px",
  });
  const submit = document.createElement("button");
  submit.type = "submit";
  submit.textContent = "Add note";
  styleButton(submit, host, true);
  form.append(input, submit);

  const list = document.createElement("ul");
  Object.assign(list.style, {
    display: "grid",
    gap: "8px",
    listStyle: "none",
    margin: "0",
    padding: "0",
  });

  function renderNotes(payload) {
    const notes = Array.isArray(payload?.notes) ? payload.notes : [];
    list.replaceChildren(
      ...notes.map((note) => {
        const item = document.createElement("li");
        item.textContent = note.text;
        Object.assign(item.style, {
          background: tokens.panelStrong,
          border: `1px solid ${tokens.border}`,
          borderRadius: tokens.radius,
          color: tokens.foreground,
          padding: "10px 12px",
        });
        return item;
      }),
    );
    status.textContent = notes.length
      ? `Showing ${notes.length} of ${payload.total} notes (page size ${limit}).`
      : "No notes yet.";
  }

  // Plugin-relative path: the helper resolves it to
  // /api/plugins/notes-console/http/notes and rejects anything that would
  // escape that prefix.
  async function refresh() {
    try {
      const payload = await host.network.json(`http/notes?limit=${limit}`);
      if (!disposed) {
        renderNotes(payload);
      }
    } catch (error) {
      if (!disposed) {
        status.textContent = `Could not load notes: ${error}`;
      }
    }
  }

  const onSubmit = async (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text) {
      return;
    }
    try {
      await host.network.json("http/notes", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text }),
      });
      input.value = "";
      await refresh();
    } catch (error) {
      host.notifications.show({
        title: "Could not add note",
        description: String(error),
        tone: "error",
      });
    }
  };
  form.addEventListener("submit", onSubmit);

  element.replaceChildren(heading, status, form, list);
  void refresh();

  return {
    unmount() {
      disposed = true;
      form.removeEventListener("submit", onSubmit);
      element.removeAttribute("style");
      element.replaceChildren();
    },
  };
}

function mountNotesActionsSection({ element, host }) {
  const { tokens } = host.appearance;
  Object.assign(element.style, {
    alignItems: "center",
    display: "flex",
    flexWrap: "wrap",
    gap: "12px",
    justifyContent: "space-between",
  });

  const copy = document.createElement("div");
  const current = document.createElement("p");
  current.textContent = `Console page size: ${notesPerPage(host)} notes`;
  Object.assign(current.style, {
    color: tokens.foreground,
    fontWeight: "600",
    margin: "0",
  });
  const guidance = document.createElement("p");
  guidance.textContent =
    "Change Notes per page with the host-rendered control below; this section does not duplicate it.";
  Object.assign(guidance.style, { color: tokens.foreground, margin: "4px 0 0" });
  copy.append(current, guidance);

  const openPage = document.createElement("button");
  openPage.type = "button";
  openPage.textContent = "Open notes page";
  styleButton(openPage, host, true);
  const navigateToPage = () => host.navigation.openPluginPage("notes");
  openPage.addEventListener("click", navigateToPage);

  element.replaceChildren(copy, openPage);

  return {
    unmount() {
      openPage.removeEventListener("click", navigateToPage);
      element.removeAttribute("style");
      element.replaceChildren();
    },
  };
}
