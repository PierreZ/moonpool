// Small helpers for every page (#415 (new site with the book), after the paros
// site.js): a copy button on code blocks, and the search of the Guide. The
// theme switch and the phone menu are in shared/theme.js.
(function () {
  document.querySelectorAll("pre.astro-code").forEach(function (pre) {
    var b = document.createElement("button");
    b.type = "button";
    b.className = "copy label";
    b.textContent = "Copy";
    b.addEventListener("click", function () {
      navigator.clipboard.writeText(pre.innerText).then(
        function () { b.textContent = "Copied"; setTimeout(function () { b.textContent = "Copy"; }, 1200); },
        function () { b.textContent = "Failed"; }
      );
    });
    var wrap = document.createElement("div");
    wrap.className = "codewrap";
    pre.parentNode.insertBefore(wrap, pre);
    wrap.appendChild(pre);
    wrap.appendChild(b);
  });

  // On a phone the sidebar of the Guide is a closed menu.
  var side = document.querySelector("[data-sidebar]");
  if (side && matchMedia("(max-width: 1000px)").matches) side.open = false;

  // The search of the Guide: Pagefind, a static index that `npm run build`
  // writes into dist/pagefind/. It loads on the first key press.
  var form = document.querySelector("form[data-search]");
  if (!form) return;
  var input = form.querySelector("input");
  var list = form.querySelector(".search-results");
  var pagefind = null, seq = 0;
  function show(items, q) {
    list.replaceChildren();
    if (q && items.length === 0) {
      var none = document.createElement("li");
      none.textContent = "No page has “" + q + "”.";
      list.append(none);
    }
    items.forEach(function (r) {
      var li = document.createElement("li");
      var a = document.createElement("a");
      a.href = r.url;
      a.textContent = (r.meta && r.meta.title) || r.url;
      var p = document.createElement("p");
      p.innerHTML = r.excerpt; // Pagefind escapes the text and adds <mark>
      li.append(a, p);
      list.append(li);
    });
  }
  input.addEventListener("input", function () {
    var q = input.value.trim(), mine = ++seq;
    if (!q) { show([], ""); return; }
    (pagefind ? Promise.resolve(pagefind) : import(form.dataset.search).then(function (m) { pagefind = m; return m; }))
      .then(function (pf) { return pf.debouncedSearch(q, {}, 200); })
      .then(function (res) {
        if (!res || mine !== seq) return null;
        return Promise.all(res.results.slice(0, 8).map(function (r) { return r.data(); }));
      })
      .then(function (items) { if (items) show(items, q); })
      .catch(function () { show([], ""); });
  });
})();
