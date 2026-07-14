import { invoke } from "@tauri-apps/api/core";
import "./pusula-app.css";
import "./styles.css";

type DesktopRequestOptions = {
  method?: string;
  body?: unknown;
  [key: string]: unknown;
};

type BusinessProfile = {
  name?: string;
  address?: string;
  phone?: string;
  website?: string;
  footer_sub?: string;
  footerSub?: string;
};

declare global {
  interface Window {
    PusulaApp: {
      apiBase: string;
      nonce: string;
      desktop: boolean;
      business: BusinessProfile;
      offline: {
        enabled: boolean;
        swUrl: string;
        assetUrls: string[];
      };
    };
    pusulaDesktopApi: (
      path: string,
      options?: DesktopRequestOptions,
    ) => Promise<unknown>;
  }
}

function normalizeBody(body: unknown): unknown {
  if (typeof body !== "string") return body ?? null;
  if (!body.trim()) return null;
  try {
    return JSON.parse(body);
  } catch {
    return body;
  }
}

window.PusulaApp = {
  apiBase: "pusula-desktop://local",
  nonce: "",
  desktop: true,
  business: {},
  offline: {
    enabled: false,
    swUrl: "",
    assetUrls: [],
  },
};

window.pusulaDesktopApi = async (path, options = {}) => {
  const method = String(options.method || "GET").toUpperCase();
  return invoke("api_request", {
    path,
    method,
    body: normalizeBody(options.body),
  });
};

async function bootstrap(): Promise<void> {
  try {
    const profile = await window.pusulaDesktopApi("/business-profile");
    if (profile && typeof profile === "object") {
      window.PusulaApp.business = profile as BusinessProfile;
    }
  } catch (error) {
    const startup = document.getElementById("pusula-startup-error");
    if (startup) {
      startup.hidden = false;
      startup.textContent = `Veritabanı açılamadı: ${String(error)}`;
    }
  }

  await import("./pusula-app.js");
}

void bootstrap();
