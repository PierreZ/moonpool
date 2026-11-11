// The sections of the site, in the order of the top bar (#414 (website
// makeover)): Techniques, Playground, Guide, Crates. A section that has no
// page yet has no link; the issue that adds the page adds its link.
export const NAV = [
  { name: "Techniques", path: "techniques/" },
  { name: "Guide", path: "guide/" },
  { name: "Crates", path: "crates/" },
] as const;

export const REPO = "https://github.com/PierreZ/moonpool";

/** A link inside the site, with the base path in front. */
export function url(path = ""): string {
  const base = import.meta.env.BASE_URL.replace(/\/?$/, "/");
  return base + path.replace(/^\//, "");
}
