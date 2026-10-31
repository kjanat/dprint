const storageKey = "dprint-theme";

type Theme = "light" | "dark";

const readPreference = (): Theme | null => {
  try {
    const value = localStorage.getItem(storageKey);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
};

export const getTheme = (): Theme => {
  const theme = document.documentElement.dataset.theme;
  if (theme === "light" || theme === "dark") return theme;
  return globalThis.matchMedia("(prefers-color-scheme: dark)").matches
    ? "dark"
    : "light";
};

export const setupTheme = () => {
  const systemTheme = globalThis.matchMedia("(prefers-color-scheme: dark)");
  const buttons = document.querySelectorAll<HTMLElement>("[data-theme-toggle]");
  let preference = readPreference();

  const update = () => {
    const theme = preference ?? (systemTheme.matches ? "dark" : "light");
    document.documentElement.dataset.theme = theme;
    const nextTheme = theme === "dark" ? "light" : "dark";
    for (const button of buttons) {
      const label = `Switch to ${nextTheme} mode`;
      button.setAttribute("aria-label", label);
      button.setAttribute("title", label);
      const text = button.querySelector("[data-theme-label]");
      if (text != null) text.textContent = nextTheme === "light" ? "Light" : "Dark";
      button.removeAttribute("hidden");
    }
    for (const meta of document.querySelectorAll("meta[name=\"theme-color\"]")) {
      meta.setAttribute(
        "content",
        getComputedStyle(document.body).backgroundColor,
      );
    }
    globalThis.dispatchEvent(new Event("dprint:theme-change"));
  };

  const toggle = () => {
    preference = getTheme() === "dark" ? "light" : "dark";
    try {
      localStorage.setItem(storageKey, preference);
    } catch {
      // The theme still works when storage is unavailable.
    }
    update();
  };
  const onStorage = (event: StorageEvent) => {
    if (event.key !== storageKey && event.key !== null) return;
    preference = readPreference();
    update();
  };

  update();
  for (const button of buttons) button.addEventListener("click", toggle);
  systemTheme.addEventListener("change", update);
  globalThis.addEventListener("storage", onStorage);

  return () => {
    for (const button of buttons) button.removeEventListener("click", toggle);
    systemTheme.removeEventListener("change", update);
    globalThis.removeEventListener("storage", onStorage);
  };
};
