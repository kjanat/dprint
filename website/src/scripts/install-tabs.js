// drives the install command tabs + copy button on the home page
const commands = {
  shell: "curl -fsSL https://dprint.kjanat.dev/install.sh | sh",
  pwsh: "irm https://dprint.kjanat.dev/install.ps1 | iex",
  npm: "NOT AVAILABLE", // npm install -g @kjanat/dprint
  brew: "NOT AVAILABLE",
  cargo: "cargo install --git https://github.com/kjanat/dprint dprint --bin dprint",
};

export function addInstallTabsEvent() {
  const tabs = document.querySelectorAll(".os-tab");
  const cmdText = document.getElementById("cmd-text");
  const copyBtn = document.getElementById("copy-btn");
  if (tabs.length === 0 || cmdText == null) return; // not on the home page

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      tabs.forEach((t) => {
        t.classList.remove("active");
      });
      tab.classList.add("active");
      const os = tab.getAttribute("data-os");
      if (commands[os] != null) cmdText.textContent = commands[os];
      if (copyBtn != null) copyBtn.textContent = "copy";
    });
  });

  if (copyBtn != null) {
    let copyTimeout;
    copyBtn.addEventListener("click", async () => {
      clearTimeout(copyTimeout);
      copyBtn.disabled = true;
      const command = cmdText.textContent;
      try {
        if (navigator.clipboard == null) {
          throw new Error("Clipboard is unavailable.");
        }
        await navigator.clipboard.writeText(command);
        if (cmdText.textContent === command) copyBtn.textContent = "copied ✓";
      } catch {
        if (cmdText.textContent === command) {
          copyBtn.textContent = "copy failed";
        }
      } finally {
        copyBtn.disabled = false;
        copyTimeout = setTimeout(() => {
          copyBtn.textContent = "copy";
        }, 1600);
      }
    });
  }
}
