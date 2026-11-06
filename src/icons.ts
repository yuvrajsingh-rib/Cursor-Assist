// 24px stroke icons drawn with currentColor, so they follow the theme.
const PATHS: Record<string, string> = {
  sparkle: "M12 3l1.9 5.8L20 11l-6.1 2.2L12 19l-1.9-5.8L4 11l6.1-2.2z",
  wand: "M15 4V2M15 16v-2M8 9h2M20 9h2M17.8 11.8 19 13M17.8 6.2 19 5M3 21l9-9M12.2 6.2 11 5",
  shorten: "M4 6h16M4 12h10M4 18h5",
  check: "M20 6 9 17l-5-5",
  tone: "M7.9 20A9 9 0 1 0 4 16.1L2 22z",
  bolt: "M13 2 3 14h9l-1 8 10-12h-9z",
  back: "M15 18l-6-6 6-6",
  close: "M18 6 6 18M6 6l12 12",
  retry: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8M3 3v5h5",
  copy: "M8 8h12v12H8zM16 8V4H4v12h4",
  mic: "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3zM19 11a7 7 0 0 1-14 0M12 18v3",
  stop: "M7 7h10v10H7z",
  send: "M5 12h14M13 6l6 6-6 6",
  clock: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 7v5l3 2",
  prompt: "M4 17l6-5-6-5M12 19h8",
};

export function icon(name: string): string {
  const d = PATHS[name] ?? PATHS.bolt;
  return `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="${d}"/></svg>`;
}

/** Fills every `[data-icon]` placeholder in the page. */
export function hydrateIcons(root: ParentNode = document) {
  root.querySelectorAll<HTMLElement>("[data-icon]").forEach((el) => {
    el.insertAdjacentHTML("afterbegin", icon(el.dataset.icon!));
  });
}
