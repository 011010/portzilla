// portzilla landing page — progressive enhancement only.
// Everything here is optional: without JS the page is fully readable.

(() => {
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  // Reveal blocks on scroll. The class is added here rather than in the markup
  // so a JS failure can never leave content invisible.
  if (!reduced && "IntersectionObserver" in window) {
    const selectors = [
      ".section-head",
      ".why-item",
      ".layer",
      ".guard-copy > *",
      ".verdict",
      ".install-card",
      ".platforms",
      ".anatomy-list li",
      ".closing h2",
      ".closing-lede",
      ".closing .hero-links",
    ];

    const observed = [];
    for (const selector of selectors) {
      for (const el of document.querySelectorAll(selector)) {
        if (observed.includes(el)) continue;
        observed.push(el);
        el.classList.add("reveal");
      }
    }

    // Stagger siblings so a row of cards arrives as a sequence, not a block.
    const groups = new Map();
    for (const el of observed) {
      const parent = el.parentElement;
      if (!parent) continue;
      const list = groups.get(parent) ?? [];
      list.push(el);
      groups.set(parent, list);
    }
    for (const list of groups.values()) {
      list.forEach((el, index) => {
        el.style.transitionDelay = `${Math.min(index * 70, 350)}ms`;
      });
    }

    const io = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          entry.target.classList.add("is-visible");
          io.unobserve(entry.target);
        }
      },
      { threshold: 0.12, rootMargin: "0px 0px -8% 0px" }
    );

    for (const el of observed) io.observe(el);
  }

  // Copy the install command.
  for (const button of document.querySelectorAll("[data-copy]")) {
    button.addEventListener("click", async () => {
      const source = document.querySelector(button.dataset.copy);
      if (!source) return;
      const text = source.textContent.trim();
      try {
        await navigator.clipboard.writeText(text);
      } catch {
        const range = document.createRange();
        range.selectNodeContents(source);
        const selection = window.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        document.execCommand("copy");
        selection.removeAllRanges();
      }
      const previous = button.textContent;
      button.textContent = "Copied";
      button.classList.add("copied");
      window.setTimeout(() => {
        button.textContent = previous;
        button.classList.remove("copied");
      }, 1600);
    });
  }
})();
