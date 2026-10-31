(() => {
  "use strict";

  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  /* ---------- Reveal on scroll ---------- */
  const revealEls = document.querySelectorAll(".reveal");
  // Stagger siblings that share a parent so grids cascade in.
  const groups = new Map();
  revealEls.forEach((el) => {
    const n = groups.get(el.parentElement) || 0;
    el.style.setProperty("--d", `${Math.min(n, 6) * 0.07}s`);
    groups.set(el.parentElement, n + 1);
  });
  if ("IntersectionObserver" in window && !reduceMotion) {
    const io = new IntersectionObserver(
      (entries) => entries.forEach((e) => {
        if (e.isIntersecting) { e.target.classList.add("in"); io.unobserve(e.target); }
      }),
      { threshold: 0.12, rootMargin: "0px 0px -40px 0px" }
    );
    revealEls.forEach((el) => io.observe(el));
  } else {
    revealEls.forEach((el) => el.classList.add("in"));
  }

  /* ---------- Nav: border on scroll, active section ---------- */
  const nav = document.querySelector(".nav");
  const links = [...document.querySelectorAll(".nav-links a")];
  const sections = links.map((a) => document.querySelector(a.getAttribute("href")));
  const timeline = document.querySelector(".timeline");
  const onScroll = () => {
    nav.classList.toggle("scrolled", window.scrollY > 8);
    let current = -1;
    sections.forEach((s, i) => { if (s && s.getBoundingClientRect().top < 120) current = i; });
    links.forEach((a, i) => a.classList.toggle("active", i === current));
    if (timeline) {
      const r = timeline.getBoundingClientRect();
      const p = (window.innerHeight * 0.75 - r.top) / r.height;
      timeline.style.setProperty("--progress", Math.max(0, Math.min(1, p)).toFixed(3));
    }
  };
  window.addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  /* ---------- Card spotlight ---------- */
  document.querySelectorAll(".card").forEach((card) => {
    card.addEventListener("pointermove", (e) => {
      const r = card.getBoundingClientRect();
      card.style.setProperty("--mx", `${e.clientX - r.left}px`);
      card.style.setProperty("--my", `${e.clientY - r.top}px`);
    });
  });

  /* ---------- Setup tabs ---------- */
  const tabs = [...document.querySelectorAll('.tabs [role="tab"]')];
  const select = (tab) => {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute("aria-selected", on);
      t.tabIndex = on ? 0 : -1;
      document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
    });
  };
  tabs.forEach((t, i) => {
    t.addEventListener("click", () => select(t));
    t.addEventListener("keydown", (e) => {
      const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (!d) return;
      e.preventDefault();
      const next = tabs[(i + d + tabs.length) % tabs.length];
      select(next);
      next.focus();
    });
  });

  /* ---------- Latest version label (works once the repo is public) ---------- */
  fetch("https://api.github.com/repos/OunissiDhiaEddine/Ultimate-Console/releases/latest")
    .then((r) => (r.ok ? r.json() : null))
    .then((rel) => {
      if (rel && /^v?\d/.test(rel.tag_name || "")) {
        document.getElementById("version-label").textContent = rel.tag_name;
      }
    })
    .catch(() => {});

  if (reduceMotion) return;

  /* Run an async loop only while its element is on screen. */
  const whenVisible = (el, loop) => {
    let visible = false;
    let running = false;
    const start = async () => {
      if (running) return;
      running = true;
      while (visible) await loop();
      running = false;
    };
    new IntersectionObserver(([e]) => {
      visible = e.isIntersecting;
      if (visible) start();
    }, { threshold: 0.3 }).observe(el);
  };

  /* ---------- Hero TV demo ---------- */
  const screen = document.getElementById("screen");
  const toast = document.getElementById("toast");
  const caption = document.getElementById("demo-caption");
  const lastKey = document.getElementById("demo-key");
  const keyEls = [...document.querySelectorAll(".keys kbd")];
  const tiles = [...document.querySelectorAll(".tiles span")];

  const say = (text) => { caption.textContent = text; };
  const showToast = async (text, ms) => {
    toast.textContent = text;
    toast.classList.add("show");
    await sleep(ms);
    toast.classList.remove("show");
  };
  const press = async (key) => {
    lastKey.textContent = key;
    for (const k of keyEls) { k.classList.add("down"); await sleep(110); }
    await sleep(320);
    keyEls.forEach((k) => k.classList.remove("down"));
  };
  const browse = async (steps) => {
    for (let i = 0; i < steps; i++) {
      tiles.forEach((t, j) => t.classList.toggle("focus", j === i % tiles.length));
      await sleep(520);
    }
  };

  whenVisible(screen, async () => {
    screen.dataset.mode = "desktop";
    tiles.forEach((t) => t.classList.remove("focus"));
    say("Your normal Windows desktop");
    await sleep(2000);

    say("Ctrl + Alt + G");
    await press("G");
    screen.dataset.mode = "waking";
    say("Waking the TV and switching displays…");
    await showToast("Waking TV…", 1100);
    await showToast("TV is the only display", 900);
    screen.dataset.mode = "console";
    say("Gaming mode: Big Picture, no desktop");
    await browse(4);

    say("Ctrl + Alt + O");
    await press("O");
    screen.dataset.mode = "overlay";
    say("Overlay: volume, task switcher, live stats");
    await sleep(2600);
    await press("O");
    screen.dataset.mode = "console";
    await browse(2);

    say("Ctrl + Alt + G again");
    await press("G");
    screen.dataset.mode = "waking";
    await sleep(500);
    screen.dataset.mode = "desktop";
    tiles.forEach((t) => t.classList.remove("focus"));
    say("Desktop, monitors and sound restored");
    await sleep(2400);
  });

  /* ---------- SmartScreen walkthrough ---------- */
  const ss = document.getElementById("smartscreen");
  if (ss) {
    const cursor = ss.querySelector(".cursor");
    const moveTo = async (el) => {
      const box = ss.getBoundingClientRect();
      const r = el.getBoundingClientRect();
      cursor.style.left = `${r.left - box.left + r.width * 0.45}px`;
      cursor.style.top = `${r.top - box.top + r.height * 0.4}px`;
      await sleep(900);
    };
    const click = async () => {
      cursor.classList.add("click");
      await sleep(150);
      cursor.classList.remove("click");
    };
    whenVisible(ss, async () => {
      ss.className = "smartscreen";
      cursor.style.left = "80%";
      cursor.style.top = "110%";
      await sleep(1200);
      await moveTo(ss.querySelector(".ss-link"));
      ss.classList.add("s-more");
      await click();
      ss.classList.add("s-open");
      await sleep(900);
      await moveTo(ss.querySelector(".ss-run"));
      await click();
      ss.classList.add("s-run");
      await sleep(2200);
    });
  }
})();
