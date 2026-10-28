import type { CSSProperties } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";

/** Use the same chevron for native selects and the searchable model picker. */
export const routeSelectStyle: CSSProperties = {
  ...inputStyle,
  appearance: "none",
  backgroundImage: 'url("data:image/svg+xml,%3Csvg xmlns=%27http://www.w3.org/2000/svg%27 width=%2712%27 height=%2712%27 viewBox=%270 0 12 12%27 fill=%27none%27%3E%3Cpath d=%27m3 4.5 3 3 3-3%27 stroke=%27%239ca3af%27 stroke-width=%271.5%27 stroke-linecap=%27round%27 stroke-linejoin=%27round%27/%3E%3C/svg%3E")',
  backgroundPosition: "right 0.75rem center",
  backgroundRepeat: "no-repeat",
  backgroundSize: "12px 12px",
  paddingRight: "2rem",
};
