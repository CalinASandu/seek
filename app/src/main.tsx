import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { isTauri } from "./api";
import "./styles/tokens.css";
import "./styles/app.css";

if (isTauri()) document.documentElement.classList.add("mica");

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
