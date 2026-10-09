import { restoreAnchorScroll } from "./scripts/anchor-scroll.ts";
import { setupDocMenu } from "./scripts/doc-menu-toggle.ts";
import { addInstallTabsEvent } from "./scripts/install-tabs.ts";
import { setupNavHeight } from "./scripts/nav-height.ts";
import { replaceConfigTable } from "./scripts/plugin-config-table-replacer.ts";
import { replacePluginUrls } from "./scripts/plugin-url-replacer.ts";
import { setupTheme } from "./scripts/theme.ts";

if (document.readyState === "complete" || document.readyState === "interactive") {
  setTimeout(onLoad, 0);
} else document.addEventListener("DOMContentLoaded", onLoad);

function onLoad() {
  setupTheme();
  setupNavHeight();
  restoreAnchorScroll();
  replacePluginUrls();
  replaceConfigTable();
  addInstallTabsEvent();
  setupDocMenu();
}
