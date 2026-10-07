const storageKey = "dprint-theme";

const readPreference = () => {
  try {
    const value = localStorage.getItem(storageKey);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
};

export const getTheme = () => {
  const theme = document.documentElement.dataset.theme;
  if (theme === "light" || theme === "dark") return theme;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
};

export const setupTheme = () => {
  const systemTheme = window.matchMedia("(prefers-color-scheme: dark)");
  const buttons = document.querySelectorAll("[data-theme-toggle]");
  let preference = readPreference();

  const update = () => {
    const theme = preference ?? (systemTheme.matches ? "dark" : "light");
    document.documentElement.dataset.theme = theme;
    const nextTheme = theme === "dark" ? "light" : "dark";
    for (const button of buttons) {
      const label = `Switch to ${nextTheme} mode`;
      button.setAttribute("aria-label", label);
      button.setAttribute("title", label);
      button.querySelector("[data-theme-label]").textContent = nextTheme === "light" ? "Light" : "Dark";
      button.removeAttribute("hidden");
    }
    for (const meta of document.querySelectorAll("meta[name=\"theme-color\"]")) {
      meta.setAttribute("content", getComputedStyle(document.body).backgroundColor);
    }
    window.dispatchEvent(new Event("dprint:theme-change"));
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
  const onStorage = (event) => {
    if (event.key !== storageKey && event.key !== null) return;
    preference = readPreference();
    update();
  };

  update();
  for (const button of buttons) button.addEventListener("click", toggle);
  systemTheme.addEventListener("change", update);
  window.addEventListener("storage", onStorage);

  return () => {
    for (const button of buttons) button.removeEventListener("click", toggle);
    systemTheme.removeEventListener("change", update);
    window.removeEventListener("storage", onStorage);
  };
};
