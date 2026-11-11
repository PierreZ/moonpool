// Copy the shared design system (website/shared/: tokens, components, theme,
// fonts) into public/shared/, and the pictures of the book (book/src/images/)
// into public/images/, so the chapters keep their `images/…` paths. Both
// targets are build outputs (ignored by git).
import { cpSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";

const copies = [
  ["../../shared/", "../public/shared/"],
  ["../../../book/src/images/", "../public/images/"],
];
for (const [from, to] of copies) {
  const src = fileURLToPath(new URL(from, import.meta.url));
  const dst = fileURLToPath(new URL(to, import.meta.url));
  rmSync(dst, { recursive: true, force: true });
  cpSync(src, dst, { recursive: true, dereference: true });
}
