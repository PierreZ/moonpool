// The icons and the social card of the site (#420 (finish)), made from the
// mark (src/components/Mark.astro) and the colours of website/shared/
// tokens.css. Run it by hand when the mark or the accent changes, then
// commit what it writes in public/:
//
//   node scripts/icons.mjs
//
// favicon.svg, favicon.ico (16, 32 and 48 px), apple-touch-icon.png (180),
// icon-192.png, icon-512.png and social-card.png (1200 by 630, with the
// astronaut of book/src/images/logo.png). The PNGs need a browser, so the
// build does not run this; `npm test` checks that favicon.svg still matches
// the mark and the tokens.
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

/** The drawing inside the `<svg>` of Mark.astro. */
export function markInner(astro) {
  const m = /<svg class="mark-moon" viewBox="0 0 20 20"[^>]*>([\s\S]*?)<\/svg>/.exec(astro);
  if (!m) throw new Error("Mark.astro has no 20 by 20 svg");
  return m[1];
}

/** The value of a colour token in the first (light) block of tokens.css. */
export function tokenColour(css, name) {
  const m = new RegExp(`--${name}: (#[0-9a-f]{3,8});`).exec(css);
  if (!m) throw new Error(`no colour token --${name}`);
  return m[1];
}

/** The favicon: the mark in the accent's text colour on a square of the accent. */
export function faviconSvg(inner, bg, fg) {
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="-3 -3 26 26"><rect x="-3" y="-3" width="26" height="26" fill="${bg}"/><g color="${fg}">${inner}</g></svg>\n`;
}

/** An ICO file that holds PNG images: `[{ size, png }]`. */
export function ico(images) {
  const head = Buffer.alloc(6 + 16 * images.length);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(images.length, 4);
  let offset = head.length;
  images.forEach(({ size, png }, i) => {
    const e = 6 + 16 * i;
    head.writeUInt8(size >= 256 ? 0 : size, e);
    head.writeUInt8(size >= 256 ? 0 : size, e + 1);
    head.writeUInt16LE(1, e + 4);
    head.writeUInt16LE(32, e + 6);
    head.writeUInt32LE(png.length, e + 8);
    head.writeUInt32LE(offset, e + 12);
    offset += png.length;
  });
  return Buffer.concat([head, ...images.map((x) => x.png)]);
}

/** The page that the social card is a picture of. */
function cardHtml(inner, css, logo) {
  const c = (n) => tokenColour(css, n);
  const font = (f) => `url(data:font/woff2;base64,${readFileSync(here(`../../shared/fonts/${f}`)).toString("base64")})`;
  return `<!doctype html><html><head><style>
@font-face { font-family: Mono; src: ${font("jetbrains-mono-latin-400-normal.woff2")}; }
@font-face { font-family: Mono; font-weight: 700; src: ${font("jetbrains-mono-latin-700-normal.woff2")}; }
html, body { margin: 0; }
body { width: 1200px; height: 630px; display: flex; background: ${c("bg")}; color: ${c("fg")}; font-family: Mono; }
.pic { width: 630px; height: 630px; border-right: 1px solid ${c("fg")}; }
.pic img { width: 630px; height: 630px; display: block; }
.text { flex: 1; padding: 56px 48px; display: flex; flex-direction: column; }
.brand { display: flex; align-items: center; gap: 16px; font-size: 40px; font-weight: 700; }
.brand svg { width: 56px; height: 56px; }
.kicker { margin-top: 40px; font-size: 20px; letter-spacing: 0.08em; text-transform: uppercase; color: ${c("muted")}; }
h1 { margin: 16px 0 0; font-size: 44px; line-height: 1.2; }
h1 i { display: inline-block; width: 0.55em; height: 0.9em; background: ${c("accent")}; vertical-align: -0.08em; }
.url { margin-top: auto; font-size: 20px; color: ${c("accent")}; }
</style></head><body>
<div class="pic"><img src="data:image/png;base64,${logo.toString("base64")}" alt=""></div>
<div class="text">
<div class="brand"><svg viewBox="0 0 20 20" color="${c("fg")}">${inner}</svg>moonpool</div>
<div class="kicker">$ deterministic simulation testing for Rust</div>
<h1>One seed, one exact run<i></i></h1>
<div class="url">pierrez.github.io/moonpool</div>
</div></body></html>`;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const inner = markInner(readFileSync(here("../src/components/Mark.astro"), "utf8"));
  const css = readFileSync(here("../../shared/tokens.css"), "utf8");
  const svg = faviconSvg(inner, tokenColour(css, "accent"), tokenColour(css, "accent-fg"));
  writeFileSync(here("../public/favicon.svg"), svg);
  const { chromium } = createRequire(import.meta.url)("playwright");
  const browser = await chromium.launch();
  const shot = async (html, width, height) => {
    const page = await browser.newPage({ viewport: { width, height }, deviceScaleFactor: 1 });
    await page.setContent(html);
    await page.waitForTimeout(200);
    const png = await page.screenshot({ omitBackground: false });
    await page.close();
    return png;
  };
  const icon = (size) => shot(`<html><body style="margin:0">${svg.replace("<svg ", `<svg width="${size}" height="${size}" `)}</body></html>`, size, size);
  const small = [];
  for (const size of [16, 32, 48]) small.push({ size, png: await icon(size) });
  writeFileSync(here("../public/favicon.ico"), ico(small));
  writeFileSync(here("../public/apple-touch-icon.png"), await icon(180));
  writeFileSync(here("../public/icon-192.png"), await icon(192));
  writeFileSync(here("../public/icon-512.png"), await icon(512));
  const logo = readFileSync(here("../../../book/src/images/logo.png"));
  writeFileSync(here("../public/social-card.png"), await shot(cardHtml(inner, css, logo), 1200, 630));
  await browser.close();
  console.log("icons and social card written in public/");
}
