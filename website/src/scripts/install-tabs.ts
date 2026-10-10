// drives the install command tabs + copy button on the home page
const commands: Record<string, string> = {
  shell: "curl -fsSL https://dprint.kjanat.dev/install.sh | sh",
  pwsh: "irm https://dprint.kjanat.dev/install.ps1 | iex",
  npm: "NOT AVAILABLE", // npm install -g kprint
  brew: "brew install kjanat/tap/dprint",
  cargo: "cargo install --locked --git https://github.com/kjanat/dprint kprint --bin dprint",
};

export function addInstallTabsEvent() {
  const tabs = document.querySelectorAll<HTMLElement>(".os-tab");
  const cmdText = document.getElementById("cmd-text");
  const copyBtn = document.querySelector<HTMLButtonElement>("#copy-btn");
  if (tabs.length === 0 || cmdText == null) return; // not on the home page

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      tabs.forEach((t) => {
        t.classList.remove("active");
      });
      tab.classList.add("active");
      const os = tab.getAttribute("data-os");
      const command = os == null ? undefined : commands[os];
      if (command != null) cmdText.textContent = command;
      if (copyBtn != null) copyBtn.textContent = "copy";
    });
  });

  if (copyBtn != null) {
    let copyTimeout: ReturnType<typeof setTimeout> | undefined;
    copyBtn.addEventListener("click", async () => {
      clearTimeout(copyTimeout);
      copyBtn.disabled = true;
      const command = cmdText.textContent ?? "";
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
