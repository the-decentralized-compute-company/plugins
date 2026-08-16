/**
 * Console bundle for `contribution-ledger`.
 *
 * Ships as-is: the host imports it as a browser ES module and does not
 * transpile TypeScript, JSX, CommonJS, or bare npm specifiers. It uses only DOM
 * APIs and the host object described in `host-contract.d.ts`.
 *
 * The page is deliberately plain about what it is showing. Every number here is
 * this node's own count of its own work, and the page says so above the fold
 * rather than in a footnote — a contribution view that looks like a wallet is
 * the exact misreading this plugin exists to avoid.
 *
 * @type {import('./host-contract').MeshPluginUiBundleModule}
 */
const moduleRegistration = {
  async registerMeshPluginUi(host) {
    host.state.update({ loadedAt: Date.now() });
    return {
      pages: { contribution: mountContributionPage },
      configSections: { "ledger-actions": mountLedgerActionsSection },
    };
  },
};
export const registerMeshPluginUi = moduleRegistration.registerMeshPluginUi;

const DEFAULT_WINDOW_DAYS = 7;
const MAX_WINDOW_DAYS = 3650;
const PEER_ROW_LIMIT = 25;

/**
 * Read the operator setting the plugin declared in its config schema. Settings
 * are host-owned: the plugin process never receives them, so the bundle reads
 * them from the host and passes the value to the plugin as a query parameter.
 */
function windowDays(host) {
  const configured = Number(
    host.config.visible.settings.default_window_days ?? DEFAULT_WINDOW_DAYS,
  );
  return Number.isFinite(configured)
    ? Math.max(1, Math.min(MAX_WINDOW_DAYS, Math.round(configured)))
    : DEFAULT_WINDOW_DAYS;
}

function showPeerIds(host) {
  return host.config.visible.settings.show_peer_ids !== false;
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
    fontSize: level === "h2" ? "1.25rem" : "1rem",
    fontWeight: "650",
    margin: "0",
  });
  return element;
}

function paragraph(host, text, muted = false) {
  const element = document.createElement("p");
  element.textContent = text;
  Object.assign(element.style, {
    color: host.appearance.tokens.foreground,
    margin: "0",
    opacity: muted ? "0.8" : "1",
  });
  return element;
}

function statTile(host, label, value, hint) {
  const { tokens } = host.appearance;
  const tile = document.createElement("div");
  Object.assign(tile.style, {
    background: tokens.panelStrong,
    border: `1px solid ${tokens.borderSoft}`,
    borderRadius: tokens.radius,
    display: "grid",
    gap: "4px",
    padding: "12px 14px",
  });

  const name = document.createElement("span");
  name.textContent = label;
  Object.assign(name.style, { fontSize: "0.8rem", opacity: "0.75" });

  const amount = document.createElement("strong");
  amount.textContent = value;
  Object.assign(amount.style, { fontSize: "1.4rem", fontWeight: "700" });

  tile.append(name, amount);
  if (hint) {
    const note = document.createElement("span");
    note.textContent = hint;
    Object.assign(note.style, { fontSize: "0.75rem", opacity: "0.7" });
    tile.append(note);
  }
  return tile;
}

function formatCount(value) {
  const numeric = Number(value ?? 0);
  return Number.isFinite(numeric) ? numeric.toLocaleString() : "—";
}

function formatHours(value) {
  const numeric = Number(value ?? 0);
  return Number.isFinite(numeric) ? `${numeric.toFixed(1)} h` : "—";
}

function mountContributionPage({ element, host, page }) {
  const { tokens } = host.appearance;
  const days = windowDays(host);
  // Cancelled on unmount so a late response cannot write into a torn-down DOM.
  let disposed = false;

  Object.assign(element.style, {
    display: "grid",
    gap: "18px",
    maxWidth: "920px",
    padding: "4px",
  });

  const title = heading(host, page.label, "h2");

  const disclaimer = document.createElement("p");
  Object.assign(disclaimer.style, {
    background: tokens.panelStrong,
    border: `1px solid ${tokens.border}`,
    borderRadius: tokens.radius,
    color: tokens.foreground,
    margin: "0",
    padding: "12px 14px",
  });
  disclaimer.textContent =
    "Self-reported local record. This node counted its own work from its own host " +
    "counters. It is not a balance, a credit, or a currency, nothing settles against " +
    "it, and it is not evidence to anyone else.";

  const status = document.createElement("p");
  status.setAttribute("aria-live", "polite");
  status.setAttribute("role", "status");
  status.textContent = "Loading contribution…";
  Object.assign(status.style, { color: tokens.foreground, margin: "0" });

  const tiles = document.createElement("div");
  Object.assign(tiles.style, {
    display: "grid",
    gap: "12px",
    gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
  });

  const caveats = panel(host);
  const peers = panel(host);

  function renderSummary(payload) {
    const totals = payload?.totals ?? {};
    const coverage = payload?.coverage ?? {};
    tiles.replaceChildren(
      statTile(
        host,
        "Served on this node",
        formatCount(totals.served_locally),
        "requests answered by this machine's own hardware",
      ),
      statTile(
        host,
        "Completion tokens",
        formatCount(totals.completion_tokens),
        "observed by the host across all targets",
      ),
      statTile(
        host,
        "Attempt time",
        formatHours(Number(totals.attempt_seconds ?? 0) / 3600),
        "busy-time estimate, not GPU utilisation",
      ),
      statTile(
        host,
        "Accepting work",
        formatHours(totals.accepting_hours),
        `of ${formatHours(totals.observed_hours)} the ledger was running`,
      ),
      statTile(
        host,
        "Routed to peers",
        formatCount(totals.served_remotely),
        "this node's consumption, not its contribution",
      ),
      statTile(
        host,
        "Peers seen",
        formatCount(payload?.peers_seen),
        "shared a mesh with this node",
      ),
    );

    const fraction = Number(coverage.observed_fraction ?? 0);
    status.textContent =
      `Last ${payload?.window?.days ?? days} day(s), ` +
      `${formatCount(coverage.buckets)} bucket(s), ` +
      `${(fraction * 100).toFixed(1)}% of the window observed.` +
      (payload?.measured === false ? " Host counters are not being sampled." : "");

    const list = document.createElement("ul");
    Object.assign(list.style, {
      display: "grid",
      gap: "8px",
      margin: "0",
      paddingLeft: "18px",
    });
    for (const caveat of payload?.caveats ?? []) {
      const item = document.createElement("li");
      item.textContent = caveat;
      list.append(item);
    }
    caveats.replaceChildren(
      heading(host, "What this does and does not say"),
      list,
    );
  }

  function renderPeers(payload) {
    const rows = Array.isArray(payload?.peers) ? payload.peers : [];
    const children = [
      heading(host, "Peers seen"),
      paragraph(host, String(payload?.note ?? ""), true),
    ];

    if (!showPeerIds(host)) {
      children.push(
        paragraph(
          host,
          `${rows.length} peer(s) recorded. Ids are hidden by the "Show peer ids" setting.`,
        ),
      );
    } else if (rows.length === 0) {
      children.push(paragraph(host, "No peers recorded in this window."));
    } else {
      const list = document.createElement("ul");
      Object.assign(list.style, {
        display: "grid",
        gap: "6px",
        listStyle: "none",
        margin: "0",
        padding: "0",
      });
      for (const row of rows) {
        const item = document.createElement("li");
        Object.assign(item.style, {
          background: tokens.panelStrong,
          border: `1px solid ${tokens.borderSoft}`,
          borderRadius: tokens.radius,
          display: "flex",
          gap: "12px",
          justifyContent: "space-between",
          padding: "8px 12px",
        });
        const id = document.createElement("code");
        id.textContent = row.peer;
        const seen = document.createElement("span");
        seen.textContent = `${formatCount(row.buckets)} bucket(s), last ${row.last_seen}`;
        Object.assign(seen.style, { opacity: "0.75" });
        item.append(id, seen);
        list.append(item);
      }
      children.push(list);
    }
    peers.replaceChildren(...children);
  }

  // Plugin-relative paths: the helper resolves them under
  // /api/plugins/contribution-ledger/http/ and rejects anything that would
  // escape that prefix.
  async function refresh() {
    try {
      const payload = await host.network.json(`http/summary?days=${days}`);
      if (!disposed) {
        renderSummary(payload);
      }
    } catch (error) {
      if (!disposed) {
        // A refusal is the expected answer when nothing has been measured, so
        // show it rather than an empty dashboard of zeroes.
        tiles.replaceChildren();
        status.textContent = `No contribution summary available: ${error}`;
        caveats.replaceChildren(
          heading(host, "Why there is nothing to show"),
          paragraph(
            host,
            "The ledger only reports work it actually measured. Open the plugin's " +
              "`status` tool, or Configuration → Plugins, to see whether the host API " +
              "is configured and reachable.",
          ),
        );
      }
    }

    try {
      const payload = await host.network.json(
        `http/peers?days=${days}&limit=${PEER_ROW_LIMIT}`,
      );
      if (!disposed) {
        renderPeers(payload);
      }
    } catch (error) {
      if (!disposed) {
        peers.replaceChildren(
          heading(host, "Peers seen"),
          paragraph(host, `Could not load peers: ${error}`),
        );
      }
    }
  }

  const reload = document.createElement("button");
  reload.type = "button";
  reload.textContent = "Refresh";
  styleButton(reload, host, true);
  const onReload = () => void refresh();
  reload.addEventListener("click", onReload);

  const flush = document.createElement("button");
  flush.type = "button";
  flush.textContent = "Write current hour to disk";
  styleButton(flush, host);
  const onFlush = async () => {
    try {
      const result = await host.network.json("http/flush", { method: "POST" });
      host.notifications.show({
        title: result?.flushed ? "Bucket written" : "Nothing to write",
        description: result?.flushed
          ? "The in-progress bucket was appended to the journal."
          : "The open bucket is empty.",
        tone: "success",
      });
      await refresh();
    } catch (error) {
      host.notifications.show({
        title: "Could not write the bucket",
        description: String(error),
        tone: "error",
      });
    }
  };
  flush.addEventListener("click", onFlush);

  const actions = document.createElement("div");
  Object.assign(actions.style, { display: "flex", flexWrap: "wrap", gap: "10px" });
  actions.append(reload, flush);

  element.replaceChildren(title, disclaimer, status, tiles, actions, caveats, peers);
  void refresh();

  return {
    unmount() {
      disposed = true;
      reload.removeEventListener("click", onReload);
      flush.removeEventListener("click", onFlush);
      element.removeAttribute("style");
      element.replaceChildren();
    },
  };
}

function mountLedgerActionsSection({ element, host }) {
  let disposed = false;

  Object.assign(element.style, {
    display: "grid",
    gap: "12px",
  });

  const summary = paragraph(host, "Checking the ledger…");
  const guidance = paragraph(
    host,
    "The two settings below are rendered by the console's own controls and affect " +
      "this page only. Where the journal lives, how often the host is sampled, and " +
      "whether peer ids are written to disk are process arguments — set them in " +
      "[[plugin]].args, not here.",
    true,
  );

  const openPage = document.createElement("button");
  openPage.type = "button";
  openPage.textContent = "Open contribution page";
  styleButton(openPage, host, true);
  const navigateToPage = () => host.navigation.openPluginPage("contribution");
  openPage.addEventListener("click", navigateToPage);

  element.replaceChildren(summary, guidance, openPage);

  void (async () => {
    try {
      const status = await host.network.json("http/status");
      if (disposed) {
        return;
      }
      const source = status?.source ?? {};
      const journal = status?.journal ?? {};
      summary.textContent =
        `Journal: ${journal.path} (${formatCount(journal.bytes)} bytes). ` +
        `Host counters: ${source.mode} — ${formatCount(source.polls_ok)} poll(s) ok, ` +
        `${formatCount(source.polls_failed)} failed.` +
        (source.last_error ? ` Last error: ${source.last_error}` : "");
    } catch (error) {
      if (!disposed) {
        summary.textContent = `Could not read ledger status: ${error}`;
      }
    }
  })();

  return {
    unmount() {
      disposed = true;
      openPage.removeEventListener("click", navigateToPage);
      element.removeAttribute("style");
      element.replaceChildren();
    },
  };
}
