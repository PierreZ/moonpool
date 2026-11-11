// paros theme switch (#804 (flat engineering redesign)), shared by the book,
// paros play and the sandbox. Load it in <head>, before the CSS paints.
//
// The reader's choice is localStorage["theme"]: "goyo-light" or "goyo-dark"
// (the names the book's Goyo toggle wrote first, so an old choice holds).
// Without a choice, tokens.css follows prefers-color-scheme.
(function () {
  var KEY = "theme";
  var LIGHT = "goyo-light";
  var DARK = "goyo-dark";
  var root = document.documentElement;

  function stored() {
    try {
      var t = localStorage.getItem(KEY);
      return t === LIGHT || t === DARK ? t : null;
    } catch (e) {
      return null;
    }
  }

  function current() {
    var t = stored();
    if (t) return t;
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? DARK : LIGHT;
  }

  // Always written, so a page can read the resolved theme from <html data-theme>
  // (a frame on a book page reads it to match the page).
  function apply(t) {
    root.setAttribute("data-theme", t);
  }

  apply(current());
  if (window.matchMedia) {
    var q = window.matchMedia("(prefers-color-scheme: dark)");
    var follow = function () {
      if (stored()) return;
      apply(current());
      window.dispatchEvent(new CustomEvent("paros-theme", { detail: current() }));
    };
    if (q.addEventListener) q.addEventListener("change", follow);
  }

  window.parosTheme = {
    isDark: function () {
      return current() === DARK;
    },
    toggle: function () {
      var next = current() === DARK ? LIGHT : DARK;
      try {
        localStorage.setItem(KEY, next);
      } catch (e) {}
      apply(next);
      window.dispatchEvent(new CustomEvent("paros-theme", { detail: next }));
      return next;
    },
  };

  // Buttons marked data-theme-toggle switch the theme.
  document.addEventListener("click", function (e) {
    var b = e.target.closest && e.target.closest("[data-theme-toggle]");
    if (b) window.parosTheme.toggle();
  });

  // The phone menu: a [data-menu-toggle] button opens the element it controls.
  document.addEventListener("click", function (e) {
    var b = e.target.closest && e.target.closest("[data-menu-toggle]");
    if (!b) return;
    var panel = document.getElementById(b.getAttribute("aria-controls"));
    if (!panel) return;
    var open = b.getAttribute("aria-expanded") !== "true";
    b.setAttribute("aria-expanded", open ? "true" : "false");
    panel.classList.toggle("open", open);
  });
})();
