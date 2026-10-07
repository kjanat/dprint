import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "../../src/theme.css";
import "./index.css";
import App from "./App.tsx";

const root = document.getElementById("root");
if (root == null) {
  throw new Error("Playground root element not found.");
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
