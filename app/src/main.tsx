import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { isTauri } from "./api";
import "@fontsource-variable/inter/opsz.css";
import "./styles/tokens.css";
import "./styles/app.css";
import "./styles/space.css";
import "./styles/clean.css";

if (isTauri()) document.documentElement.classList.add("mica");

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
