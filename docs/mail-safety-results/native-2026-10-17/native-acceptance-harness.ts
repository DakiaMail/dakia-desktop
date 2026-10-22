import { emit } from "@tauri-apps/api/event";
import { api } from "/src/api.ts";

const accountId = "3b77bc44-c1f6-4f89-8dc5-176b4361c571";
const messageId = "demo-mail-safety";

const panel = document.createElement("aside");
panel.setAttribute("aria-label", "Native acceptance controls");
panel.style.cssText = [
  "position:fixed",
  "right:12px",
  "bottom:12px",
  "z-index:2147483647",
  "display:grid",
  "gap:6px",
  "padding:10px",
  "border:2px solid #244d46",
  "border-radius:8px",
  "background:#fff",
  "color:#111",
  "font:13px -apple-system,sans-serif",
].join(";");

const status = document.createElement("output");
status.setAttribute("aria-label", "Native acceptance status");
status.textContent = "Harness ready";

function button(label: string, action: () => Promise<void>) {
  const control = document.createElement("button");
  control.type = "button";
  control.textContent = label;
  control.setAttribute("aria-label", label);
  control.addEventListener("click", () => {
    status.textContent = `${label}: running`;
    void action().then(
      () => {
        status.textContent = `${label}: complete`;
      },
      (error) => {
        status.textContent = `${label}: failed: ${String(error)}`;
      },
    );
  });
  return control;
}

panel.append(
  button("Prepare fixture starred without event", async () => {
    await api.setStarred(messageId, true);
  }),
  button("Emit native mail changed", async () => {
    await emit("mail-changed", { accountId });
  }),
  button("Prepare fixture unstarred without event", async () => {
    await api.setStarred(messageId, false);
  }),
  button("Emit native mail changed inverse", async () => {
    await emit("mail-changed", { accountId });
  }),
  status,
);

document.body.append(panel);
