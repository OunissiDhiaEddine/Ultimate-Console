/*
 * Ultimate Console tab for Steam's Quick Access Menu (Steam + A in Big
 * Picture and in the in-game overlay).
 *
 * uc-agent evaluates this file in Steam's "SharedJSContext" over the CEF
 * DevTools port (see crates/uc-steamui). Plain JavaScript, no build step.
 * `__UC_VERSION__` and `__UC_BINDING__` are filled in by the agent.
 *
 * How it hooks in:
 *  1. Steam's UI is a React app bundled with webpack. We get webpack's module
 *     cache by pushing a fake chunk onto `webpackChunksteamui`.
 *  2. The Quick Access Menu component is a React.memo whose source mentions
 *     `QuickAccessMenuBrowserView`. We wrap its render function; on every
 *     render we look for the element that receives the `tabs` array (a level
 *     or two down) and append our tab to it. Instances that are already on
 *     screen are switched over through their React fibers.
 *  3. If that lookup fails (a Steam update renamed things), we look for any
 *     mounted component that renders a `tabs` array of QAM-like tabs and wrap
 *     that instead.
 *  4. The panel uses Steam's own SliderField / ToggleField / ButtonItem /
 *     Focusable when they can be found, so the controller moves through it
 *     like any other tab; otherwise simpler stand-ins built from Focusable.
 *
 * Everything Steam-internal is looked up defensively. `window.__uc.health()`
 * reports what was found; the agent logs it and the settings app shows it.
 *
 * The page talks to the agent through a DevTools binding: calling
 * `window[BINDING](json)` raises Runtime.bindingCalled in the agent, which
 * answers by evaluating `window.__uc.reply({...})`.
 */
(() => {
  'use strict';

  const VERSION = '__UC_VERSION__';
  const BINDING = '__UC_BINDING__';
  /** Our Quick Access Menu tab id. Steam's own tabs use small numbers. */
  const TAB_KEY = 7331;
  const TITLE = 'Ultimate Console';

  const previous = window.__uc;
  if (previous && previous.version === VERSION && typeof previous.health === 'function') {
    try {
      previous.ensure();
      return previous.health();
    } catch (e) {
      /* fall through and start over */
    }
  }
  if (previous && typeof previous.dispose === 'function') {
    try {
      previous.dispose();
    } catch (e) {
      /* ignore */
    }
  }

  const uc = {
    version: VERSION,
    patched: false,
    how: null,
    problems: [],
    found: {},
    lastTabRender: 0,
    timers: [],
    undo: [],
  };

  // ---------------------------------------------------------------- bridge

  const pending = new Map();
  let nextId = 1;

  function send(msg) {
    const fn = window[BINDING];
    if (typeof fn !== 'function') throw new Error('Ultimate Console agent is not connected');
    fn(JSON.stringify(msg));
  }

  /** Calls the agent. Resolves with its answer, rejects on error/timeout. */
  function call(method, params) {
    return new Promise((resolve, reject) => {
      const id = nextId++;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error('the agent did not answer'));
      }, 8000);
      pending.set(id, { resolve, reject, timer });
      try {
        send({ id, method, params: params || {} });
      } catch (e) {
        clearTimeout(timer);
        pending.delete(id);
        reject(e);
      }
    });
  }

  /** Called by the agent with `{id, result}` or `{id, error}`. */
  function reply(msg) {
    const p = msg && pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    clearTimeout(p.timer);
    if (msg.error != null) p.reject(new Error(String(msg.error)));
    else p.resolve(msg.result);
  }

  /** Events pushed by the agent (`window.__uc.push(name, data)`). */
  const listeners = new Set();
  function push(name, data) {
    for (const l of listeners) {
      try {
        l(name, data);
      } catch (e) {
        /* ignore */
      }
    }
  }

  function log(level, message) {
    try {
      // id 0: no answer wanted.
      send({ id: 0, method: 'log', params: { level, message: String(message) } });
    } catch (e) {
      /* agent not there */
    }
  }

  const reported = new Set();
  function problem(what, err) {
    const text = err ? `${what}: ${err && err.message ? err.message : err}` : what;
    if (reported.has(text)) return;
    reported.add(text);
    uc.problems.push(text);
    if (uc.problems.length > 20) uc.problems.shift();
    log('warn', text);
  }

  // --------------------------------------------------------------- webpack

  let wpRequire = null;
  let moduleCount = -1;
  let exportsCache = [];

  function webpackRequire() {
    if (wpRequire) return wpRequire;
    const chunks = window.webpackChunksteamui;
    if (!chunks || typeof chunks.push !== 'function') return null;
    try {
      chunks.push([['uc-' + Math.random()], {}, (r) => { wpRequire = r; }]);
    } catch (e) {
      problem('webpack hook failed', e);
    }
    return wpRequire;
  }

  /** Every export of every loaded module (cached until more modules load). */
  function allExports() {
    const req = webpackRequire();
    if (!req || !req.c) return [];
    const ids = Object.keys(req.c);
    if (ids.length === moduleCount) return exportsCache;
    moduleCount = ids.length;
    const out = [];
    for (const id of ids) {
      const m = req.c[id] && req.c[id].exports;
      if (!m || (typeof m !== 'object' && typeof m !== 'function')) continue;
      out.push(m);
      let keys;
      try {
        keys = Object.keys(m);
      } catch (e) {
        continue;
      }
      for (const k of keys) {
        let v;
        try {
          v = m[k];
        } catch (e) {
          continue;
        }
        if (v && (typeof v === 'object' || typeof v === 'function') && v !== m) out.push(v);
      }
    }
    exportsCache = out;
    return out;
  }

  function findExport(pred) {
    for (const e of allExports()) {
      try {
        if (pred(e)) return e;
      } catch (err) {
        /* keep looking */
      }
    }
    return null;
  }

  const fnSource = (f) => {
    try {
      return typeof f === 'function' ? Function.prototype.toString.call(f) : '';
    } catch (e) {
      return '';
    }
  };

  /** Source of a component: plain function, forwardRef or memo. */
  function componentSource(c, depth) {
    if (!c || (depth || 0) > 3) return '';
    if (typeof c === 'function') return fnSource(c);
    if (typeof c !== 'object') return '';
    return fnSource(c.render) || componentSource(c.type, (depth || 0) + 1);
  }

  // ----------------------------------------------------------------- React

  let React = null;
  function react() {
    if (React) return React;
    const r = window.SP_REACT;
    React = r && r.createElement && r.useState ? r : findExport((e) => e.createElement && e.useState && e.Component);
    if (!React) problem('React not found');
    return React;
  }

  const h = (...args) => react().createElement(...args);

  // ------------------------------------------------------ Steam components

  /** Steam's UI pieces, found by marker strings in their source. */
  const K = {};

  let kitModuleCount = -1;
  function resolveKit() {
    // Only look again when Steam has loaded more code.
    allExports();
    if (kitModuleCount === moduleCount) return;
    kitModuleCount = moduleCount;
    const looks = (name) => (e) => {
      const s = componentSource(e);
      return s.includes(name + ',fallback') || (s.includes('"' + name + '"') && s.includes('fallback'));
    };
    const wanted = {
      Focusable: (e) => {
        const s = componentSource(e);
        return typeof e === 'object' && s.includes('"flow-children"') && s.includes('onActivate') && s.includes('focusClassName');
      },
      SliderField: looks('SliderField'),
      ToggleField: looks('ToggleField'),
      ButtonItem: (e) => looks('ButtonField')(e) || componentSource(e).includes('childrenContainerWidth:"min"'),
      Field: (e) => {
        const s = componentSource(e);
        return s.includes('"shift-children-below"') || /[^A-Za-z]Field,fallback/.test(s);
      },
    };
    for (const [name, pred] of Object.entries(wanted)) {
      if (K[name]) continue;
      K[name] = findExport(pred);
      uc.found[name] = !!K[name];
    }
    if (!K.menuStore) {
      K.menuStore = findMenuStore();
      uc.found.MenuStore = !!K.menuStore;
    }
  }

  /** The object with OpenQuickAccessMenu / CloseSideMenus, if we can reach it. */
  function findMenuStore() {
    const candidates = [window.SteamUIStore];
    const router = findExport((e) => e.WindowStore && (e.Navigate || e.NavigationManager));
    if (router) candidates.push(router);
    for (const c of candidates) {
      try {
        const ws = c && c.WindowStore;
        const inst = ws && (ws.GamepadUIMainWindowInstance || ws.MainWindowInstance);
        const ms = inst && (inst.MenuStore || inst.m_MenuStore);
        if (ms && typeof ms.OpenQuickAccessMenu === 'function') return ms;
      } catch (e) {
        /* keep looking */
      }
    }
    return null;
  }

  // -------------------------------------------------------- tree utilities

  function findInTree(node, pred, depth) {
    depth = depth || 0;
    if (!node || depth > 80) return null;
    if (Array.isArray(node)) {
      for (const c of node) {
        const r = findInTree(c, pred, depth + 1);
        if (r) return r;
      }
      return null;
    }
    if (typeof node !== 'object') return null;
    try {
      if (pred(node)) return node;
    } catch (e) {
      /* ignore */
    }
    if (node.props) return findInTree(node.props.children, pred, depth + 1);
    return null;
  }

  /** Component elements in a render result, outermost first. */
  function componentElements(node, out, depth) {
    out = out || [];
    depth = depth || 0;
    if (!node || depth > 40 || out.length > 200) return out;
    if (Array.isArray(node)) {
      for (const c of node) componentElements(c, out, depth + 1);
      return out;
    }
    if (typeof node !== 'object') return out;
    if (node.type && typeof node.type !== 'string') out.push(node);
    if (node.props) componentElements(node.props.children, out, depth + 1);
    return out;
  }

  const isQamTab = (t) => t && typeof t === 'object' && 'key' in t && ('panel' in t || 'tab' in t);
  const holdsTabs = (n) => n.props && Array.isArray(n.props.tabs) && n.props.tabs.some(isQamTab);

  /**
   * Wraps components so `after(args, result)` sees each render. One wrapper
   * per original, so React keeps seeing the same component type and
   * doesn't remount anything.
   */
  function makeWrapper(name, after) {
    const cache = new WeakMap();
    const wrapFn = (orig) => {
      const f = function ucWrapped(...args) {
        const ret = orig.apply(this, args);
        try {
          const r = after(args, ret);
          return r === undefined ? ret : r;
        } catch (e) {
          problem(name, e);
          return ret;
        }
      };
      try {
        Object.assign(f, orig);
      } catch (e) {
        /* ignore */
      }
      f.__ucOriginal = orig;
      return f;
    };
    const wrap = (type) => {
      if (!type || type.__ucOriginal || type.__ucWrapped) return type;
      if (cache.has(type)) return cache.get(type);
      let out = type;
      if (typeof type === 'function' && !(type.prototype && type.prototype.isReactComponent)) {
        out = wrapFn(type);
      } else if (typeof type === 'object' && type.$$typeof && type.type) {
        out = Object.assign({}, type, { type: wrap(type.type), __ucWrapped: true });
      } else if (typeof type === 'object' && type.$$typeof && typeof type.render === 'function') {
        out = Object.assign({}, type, { render: wrapFn(type.render), __ucWrapped: true });
      }
      cache.set(type, out);
      return out;
    };
    return wrap;
  }

  // ------------------------------------------------------------ the hook

  /** Puts our tab into the `tabs` prop found in `tree`. */
  function addTab(tree, visible) {
    const holder = findInTree(tree, holdsTabs);
    if (!holder) return false;
    const tabs = holder.props.tabs;
    if (!tabs.some((t) => t && t.key === TAB_KEY)) {
      const tab = {
        key: TAB_KEY,
        title: TITLE,
        tab: h(TabIcon, null),
        panel: h(Panel, null),
        initialVisibility: visible !== false,
      };
      if (Object.isFrozen(tabs)) holder.props.tabs = tabs.concat([tab]);
      else tabs.push(tab);
    }
    uc.lastTabRender = Date.now();
    if (!uc.patched) {
      uc.patched = true;
      log('info', `tab added to the Quick Access Menu (${uc.how})`);
    }
    return true;
  }

  /**
   * After-render hooks, three levels deep. Level n looks for the `tabs`
   * holder in what was rendered; failing that it wraps the child
   * components that look like they deal with tabs, whose renders go to
   * level n + 1.
   */
  const levels = [];
  for (let i = 0; i < 3; i++) {
    levels.push(
      makeWrapper(`qam level ${i}`, (args, ret) => {
        const visible = args[0] && typeof args[0].visible === 'boolean' ? args[0].visible : undefined;
        if (addTab(ret, visible) || i + 1 >= levels.length) return ret;
        for (const el of componentElements(ret)) {
          const s = componentSource(el.type);
          if (el.props && (typeof el.props.onFocusNavDeactivated === 'function' || s.includes('tabs'))) {
            const wrapped = levels[i + 1](el.type);
            if (wrapped !== el.type) {
              try {
                el.type = wrapped;
              } catch (e) {
                /* frozen element (dev build); skip */
              }
            }
          }
        }
        return ret;
      }),
    );
  }

  /** Every document Steam's UI renders into: this one and its popups (Big Picture, overlays). */
  function documents() {
    const docs = new Set([document]);
    try {
      const pm = window.g_PopupManager;
      let popups = [];
      if (pm && typeof pm.GetPopups === 'function') popups = Array.from(pm.GetPopups() || []);
      else if (pm && pm.m_mapPopups && typeof pm.m_mapPopups.values === 'function') popups = Array.from(pm.m_mapPopups.values());
      for (const p of popups) {
        try {
          const w = p && (p.m_popup || p.window || p.m_window);
          if (w && w.document) docs.add(w.document);
        } catch (e) {
          /* closed popup */
        }
      }
    } catch (e) {
      /* no popup manager */
    }
    return Array.from(docs);
  }

  function rootFibers() {
    const roots = new Set();
    for (const doc of documents()) {
      const els = [];
      try {
        if (doc.getElementById) els.push(doc.getElementById('root'));
        if (doc.body) els.push(doc.body, ...Array.from(doc.body.children));
      } catch (e) {
        continue;
      }
      for (const el of els) {
        if (!el) continue;
        for (const k of Object.keys(el)) {
          let f = null;
          if (k.startsWith('__reactContainer$')) f = el[k];
          else if (k.startsWith('__reactFiber$')) f = el[k];
          if (!f) continue;
          while (f.return) f = f.return;
          roots.add(f);
        }
      }
    }
    return Array.from(roots);
  }

  function walkFibers(visit) {
    const stack = rootFibers();
    const seen = new Set();
    let n = 0;
    while (stack.length && n++ < 400000) {
      const f = stack.pop();
      if (!f || seen.has(f)) continue;
      seen.add(f);
      if (visit(f) === false) return;
      if (f.sibling) stack.push(f.sibling);
      if (f.child) stack.push(f.child);
    }
  }

  /** Points mounted fibers rendering `orig` at `wrapped`. Returns how many. */
  function swapFiberTypes(orig, wrapped) {
    let count = 0;
    walkFibers((f) => {
      if (f.type === orig) {
        f.type = wrapped;
        if (f.alternate) f.alternate.type = wrapped;
        count++;
        uc.undo.push(() => {
          if (f.type === wrapped) f.type = orig;
          if (f.alternate && f.alternate.type === wrapped) f.alternate.type = orig;
        });
      }
    });
    return count;
  }

  /** Strategy 1: the memo'd QuickAccessMenu component, patched at its source. */
  function patchQamModule() {
    const qam = findExport(
      (e) => typeof e === 'object' && e.$$typeof && typeof e.type === 'function' && fnSource(e.type).includes('QuickAccessMenuBrowserView'),
    );
    if (!qam) return false;
    const orig = qam.type.__ucOriginal || qam.type;
    const wrapped = levels[0](orig);
    if (qam.type !== wrapped) {
      qam.type = wrapped;
      uc.undo.push(() => {
        if (qam.type === wrapped) qam.type = orig;
      });
    }
    uc.how = 'QuickAccessMenu component';
    swapFiberTypes(orig, wrapped);
    return true;
  }

  /** Strategy 2: find a mounted `tabs` holder and wrap whoever renders it. */
  function patchFromFibers() {
    let holderFiber = null;
    walkFibers((f) => {
      const p = f.memoizedProps;
      if (p && Array.isArray(p.tabs) && p.tabs.some(isQamTab)) {
        holderFiber = f;
        return false;
      }
      return true;
    });
    if (!holderFiber) return false;
    // The component that created the holder: nearest function-component ancestor.
    let owner = holderFiber.return;
    while (owner && !(typeof owner.type === 'function' || (owner.type && typeof owner.type.render === 'function'))) {
      owner = owner.return;
    }
    if (!owner) return false;
    const orig = owner.type;
    const wrapped = levels[0](orig);
    if (wrapped === orig) return false;
    swapFiberTypes(orig, wrapped);
    // Future mounts: patch a memo / forwardRef export that holds it.
    const holder = findExport((e) => typeof e === 'object' && (e.type === orig || e.render === orig));
    if (holder) {
      const key = holder.type === orig ? 'type' : 'render';
      const w = key === 'type' ? wrapped : levels[0]({ $$typeof: holder.$$typeof, render: orig }).render;
      holder[key] = w;
      uc.undo.push(() => {
        if (holder[key] === w) holder[key] = orig;
      });
    }
    uc.how = 'mounted Quick Access Menu';
    return true;
  }

  let hookedBy = null;
  let qamModuleCount = -1;
  let fiberTries = 0;
  /**
   * Tries to hook the Quick Access Menu; cheap once it has worked. The
   * module search runs again only when Steam loads more code, the fiber
   * walk (heavier) every fifth call.
   */
  function ensure() {
    if (!webpackRequire() || !react()) return false;
    resolveKit();
    if (hookedBy) return true;
    try {
      if (qamModuleCount !== moduleCount) {
        qamModuleCount = moduleCount;
        if (patchQamModule()) hookedBy = 'module';
      }
      if (!hookedBy && fiberTries++ % 5 === 0 && patchFromFibers()) hookedBy = 'fiber';
    } catch (e) {
      problem('hooking the Quick Access Menu failed', e);
    }
    return !!hookedBy;
  }

  function health() {
    return {
      version: VERSION,
      webpack: !!wpRequire,
      react: !!React,
      hooked: !!hookedBy,
      how: uc.how,
      tabShown: uc.patched,
      lastTabRender: uc.lastTabRender,
      found: Object.assign({}, uc.found),
      problems: uc.problems.slice(),
    };
  }

  /** Opens the Quick Access Menu on our tab. Returns whether it could. */
  function open() {
    const ms = K.menuStore || (K.menuStore = findMenuStore());
    if (!ms) return false;
    try {
      ms.OpenQuickAccessMenu(TAB_KEY);
      return true;
    } catch (e) {
      problem('OpenQuickAccessMenu failed', e);
      return false;
    }
  }

  function closeMenus() {
    try {
      const ms = K.menuStore;
      if (ms && typeof ms.CloseSideMenus === 'function') ms.CloseSideMenus();
    } catch (e) {
      /* ignore */
    }
  }

  function dispose() {
    for (const t of uc.timers) clearInterval(t);
    for (const u of uc.undo.reverse()) {
      try {
        u();
      } catch (e) {
        /* ignore */
      }
    }
    for (const p of pending.values()) clearTimeout(p.timer);
    pending.clear();
    if (window.__uc === api) delete window.__uc;
  }

  // ----------------------------------------------------------------- panel

  const CSS = `
    .uc-root { padding: 0 0 12px; font-size: 14px; }
    .uc-title { text-transform: uppercase; font-size: 12px; font-weight: 700; letter-spacing: .04em;
      color: #b8bcbf; padding: 14px 16px 4px; }
    .uc-line { display: flex; justify-content: space-between; gap: 8px; padding: 6px 16px; color: #dcdedf; }
    .uc-line .uc-dim, .uc-dim { color: #8b929a; }
    .uc-sub { font-size: 12px; color: #8b929a; padding: 0 16px 4px; }
    .uc-row { display: flex; flex-wrap: wrap; gap: 6px; padding: 4px 16px 8px; }
    .uc-btn { background: rgba(255,255,255,.08); color: #dcdedf; border-radius: 3px; padding: 6px 12px;
      font-size: 13px; cursor: pointer; user-select: none; text-align: center; min-width: 32px; }
    .uc-btn.uc-wide { flex: 1; }
    .uc-btn.uc-on { background: #1a9fff; color: #fff; }
    .uc-btn.uc-danger { background: rgba(214,62,62,.35); }
    .uc-btn.uc-focus, .uc-btn.gpfocus, .uc-btn:hover { background: #dcdedf; color: #0e141b; }
    .uc-bar { height: 4px; background: rgba(255,255,255,.12); border-radius: 2px; margin: 0 16px 8px; }
    .uc-bar > div { height: 100%; background: #1a9fff; border-radius: 2px; }
    .uc-bar.uc-low > div { background: #d63e3e; }
    .uc-error { color: #ff8a8a; padding: 8px 16px; }
  `;

  /**
   * Steam may report one press as both onActivate (controller) and
   * onClick (mouse/touch); run the action once.
   */
  function once(fn) {
    let last = 0;
    return (...args) => {
      const now = Date.now();
      if (!fn || now - last < 300) return;
      last = now;
      fn(...args);
    };
  }

  function Focus(props) {
    const { children, onActivate, className, ...rest } = props;
    const run = once(onActivate);
    if (K.Focusable) {
      return h(K.Focusable, Object.assign({ className, onActivate: run, onClick: run, focusClassName: 'uc-focus' }, rest), children);
    }
    return h('div', { className, onClick: run, tabIndex: 0 }, children);
  }

  function Btn({ children, onClick, wide, on, danger }) {
    const cls = ['uc-btn', wide && 'uc-wide', on && 'uc-on', danger && 'uc-danger'].filter(Boolean).join(' ');
    return h(Focus, { className: cls, onActivate: onClick }, children);
  }

  function Section({ title, children }) {
    return h('div', null, h('div', { className: 'uc-title' }, title), children);
  }

  /** A row of buttons the D-pad moves across. */
  function Buttons({ children }) {
    if (K.Focusable) return h(K.Focusable, { className: 'uc-row', 'flow-children': 'horizontal' }, children);
    return h('div', { className: 'uc-row' }, children);
  }

  function Line({ label, value }) {
    if (K.Field) return h(K.Field, { label, bottomSeparator: 'none' }, h('span', { className: 'uc-dim' }, value));
    return h('div', { className: 'uc-line' }, h('span', null, label), h('span', { className: 'uc-dim' }, value));
  }

  /** Volume-style 0..1 slider: Steam's SliderField, or label + [−] [+]. */
  function Slider({ label, value, onChange }) {
    const pct = Math.round((value || 0) * 100);
    if (K.SliderField) {
      return h(K.SliderField, {
        label,
        value: pct,
        min: 0,
        max: 100,
        step: 5,
        showValue: true,
        valueSuffix: '%',
        bottomSeparator: 'none',
        onChange: (v) => onChange(Math.max(0, Math.min(100, v)) / 100),
      });
    }
    return h(
      'div',
      null,
      h('div', { className: 'uc-line' }, h('span', null, label), h('span', { className: 'uc-dim' }, pct + '%')),
      h(
        Buttons,
        null,
        h(Btn, { wide: true, onClick: () => onChange(Math.max(0, pct - 5) / 100) }, '−'),
        h(Btn, { wide: true, onClick: () => onChange(Math.min(100, pct + 5) / 100) }, '+'),
      ),
    );
  }

  function Toggle({ label, checked, onChange }) {
    if (K.ToggleField) return h(K.ToggleField, { label, checked: !!checked, bottomSeparator: 'none', onChange: (v) => onChange(!!v) });
    return h(Buttons, null, h(Btn, { wide: true, on: checked, onClick: () => onChange(!checked) }, `${label}: ${checked ? 'On' : 'Off'}`));
  }

  function Action({ label, button, description, onClick, disabled }) {
    if (K.ButtonItem) {
      return h(K.ButtonItem, { label, description, layout: 'below', bottomSeparator: 'none', disabled, onClick }, button);
    }
    return h(
      'div',
      null,
      label && h('div', { className: 'uc-line' }, h('span', null, label)),
      description && h('div', { className: 'uc-sub' }, description),
      h(Buttons, null, h(Btn, { wide: true, onClick: disabled ? () => {} : onClick }, button)),
    );
  }

  /** Steam-style percentage bar (batteries). */
  function Bar({ fraction, low }) {
    if (fraction == null) return null;
    return h('div', { className: 'uc-bar' + (low ? ' uc-low' : '') }, h('div', { style: { width: Math.round(fraction * 100) + '%' } }));
  }

  /**
   * Remembers a value the user just set for a moment, so the next poll
   * (which may still have the old value) doesn't make the slider jump.
   */
  function useOverrides() {
    const R = react();
    const [, force] = R.useState(0);
    const ref = R.useRef(new Map());
    return {
      get(key, fallback) {
        const o = ref.current.get(key);
        return o && Date.now() - o.at < 1500 ? o.value : fallback;
      },
      set(key, value) {
        ref.current.set(key, { value, at: Date.now() });
        force((n) => n + 1);
      },
    };
  }

  /** Two presses within 3 s: the first arms, the second does it. */
  function useConfirm() {
    const R = react();
    const [armed, setArmed] = R.useState(null);
    R.useEffect(() => {
      if (!armed) return undefined;
      const t = setTimeout(() => setArmed(null), 3000);
      return () => clearTimeout(t);
    }, [armed]);
    return {
      armed,
      press(key, action) {
        if (armed === key) {
          setArmed(null);
          action();
        } else setArmed(key);
      },
    };
  }

  function Panel() {
    const R = react();
    const [state, setState] = R.useState(null);
    const [error, setError] = R.useState(null);
    const rootRef = R.useRef(null);
    const pollRef = R.useRef(null);
    const ov = useOverrides();
    const confirm = useConfirm();

    const refresh = R.useCallback(async () => {
      try {
        const s = await call('state');
        setState(s);
        setError(null);
      } catch (e) {
        setError(e.message || String(e));
      }
    }, []);
    pollRef.current = refresh;

    R.useEffect(() => {
      let alive = true;
      let timer = null;
      const shown = () => {
        const el = rootRef.current;
        if (!el || !el.isConnected) return false;
        const doc = el.ownerDocument;
        return el.getClientRects().length > 0 && (!doc || doc.visibilityState !== 'hidden');
      };
      const tick = async () => {
        if (!alive) return;
        if (shown()) await pollRef.current();
        if (alive) timer = setTimeout(tick, 1000);
      };
      tick();
      const onPush = (name) => {
        if (name === 'refresh') pollRef.current();
      };
      listeners.add(onPush);
      return () => {
        alive = false;
        clearTimeout(timer);
        listeners.delete(onPush);
      };
    }, []);

    // Sliders send at most one change per 80 ms, always the latest.
    const throttled = R.useRef(new Map());
    const sendThrottled = (key, method, params) => {
      const t = throttled.current;
      const entry = t.get(key) || { busy: false, next: null };
      t.set(key, entry);
      entry.next = { method, params };
      if (entry.busy) return;
      const pump = () => {
        const job = entry.next;
        entry.next = null;
        if (!job) {
          entry.busy = false;
          return;
        }
        entry.busy = true;
        call(job.method, job.params)
          .catch((e) => setError(e.message || String(e)))
          .finally(() => setTimeout(pump, 80));
      };
      pump();
    };

    const act = (method, params, after) =>
      call(method, params)
        .then(() => {
          if (after) after();
          return refresh();
        })
        .catch((e) => setError(e.message || String(e)));

    const kids = [h('style', { key: 'css' }, CSS)];
    if (error && !state) {
      kids.push(h('div', { key: 'err', className: 'uc-error' }, `Can't reach Ultimate Console: ${error}`));
    } else if (!state) {
      kids.push(h('div', { key: 'wait', className: 'uc-sub', style: { padding: 16 } }, 'Loading…'));
    } else {
      kids.push(...sections(state, { ov, confirm, act, sendThrottled }));
      if (error) kids.push(h('div', { key: 'err2', className: 'uc-error' }, error));
    }
    return h('div', { className: 'uc-root', ref: rootRef }, kids);
  }

  function sections(s, { ov, confirm, act, sendThrottled }) {
    const out = [];
    const host = s.host || {};

    // Console: mode, TV and leaving gaming mode.
    const consoleKids = [h(Line, { key: 'mode', label: 'Mode', value: host.mode || 'Unknown' })];
    if (host.tv) consoleKids.push(h(Line, { key: 'tv', label: 'TV', value: host.tv }));
    if (host.gaming) {
      consoleKids.push(
        h(Action, {
          key: 'leave',
          button: confirm.armed === 'leave' ? 'Press again to leave' : 'Leave gaming mode',
          description: 'Back to the desktop: monitors, Explorer and the TV as they were.',
          onClick: () => confirm.press('leave', () => act('leaveGaming')),
        }),
      );
    }
    out.push(h(Section, { key: 'console', title: 'Console' }, consoleKids));

    // Volume.
    const vol = [];
    if (s.volume) {
      vol.push(
        h(Slider, {
          key: 'master',
          label: 'Volume',
          value: ov.get('master', s.volume.level),
          onChange: (v) => {
            ov.set('master', v);
            sendThrottled('master', 'setVolume', { level: v });
          },
        }),
        h(Toggle, {
          key: 'mute',
          label: 'Mute',
          checked: ov.get('mute', s.volume.muted),
          onChange: (m) => {
            ov.set('mute', m);
            act('setMuted', { muted: m });
          },
        }),
      );
    } else {
      vol.push(h('div', { key: 'novol', className: 'uc-sub' }, s.volumeError || 'No audio output'));
    }
    for (const app of s.apps || []) {
      const key = 'app' + app.pid;
      vol.push(
        h(Slider, {
          key,
          label: app.name + (app.muted ? ' (muted)' : ''),
          value: ov.get(key, app.level),
          onChange: (v) => {
            ov.set(key, v);
            sendThrottled(key, 'setAppVolume', { pid: app.pid, level: v });
          },
        }),
      );
    }
    out.push(h(Section, { key: 'volume', title: 'Volume' }, vol));

    // Output device.
    if ((s.outputs || []).length > 1) {
      out.push(
        h(
          Section,
          { key: 'outputs', title: 'Sound output' },
          s.outputs.map((d) =>
            h(Action, {
              key: d.id,
              label: d.name,
              button: d.default ? 'In use' : 'Use this',
              disabled: d.default,
              onClick: () => act('setOutput', { id: d.id }),
            }),
          ),
        ),
      );
    }

    // Windows.
    const tasks = s.tasks || [];
    out.push(
      h(
        Section,
        { key: 'tasks', title: 'Windows' },
        tasks.length === 0
          ? h('div', { className: 'uc-sub' }, 'No other windows are open.')
          : tasks.map((t) =>
              h(
                'div',
                { key: t.hwnd },
                h('div', { className: 'uc-line' }, h('span', null, t.title)),
                h('div', { className: 'uc-sub' }, t.detail),
                h(
                  Buttons,
                  null,
                  h(Btn, { wide: true, onClick: () => act('switchTo', { hwnd: t.hwnd }, closeMenus) }, 'Switch to'),
                  h(Btn, { onClick: () => act('closeWindow', { hwnd: t.hwnd }) }, 'Close'),
                  h(
                    Btn,
                    {
                      danger: confirm.armed === 'kill' + t.pid,
                      onClick: () => confirm.press('kill' + t.pid, () => act('endProcess', { pid: t.pid })),
                    },
                    confirm.armed === 'kill' + t.pid ? 'Sure?' : 'End',
                  ),
                ),
              ),
            ),
      ),
    );

    // Performance.
    const sys = s.system || {};
    const perf = [
      h(Line, { key: 'cpu', label: 'CPU', value: sys.cpu || '—' }),
      h(Line, { key: 'gpu', label: 'GPU', value: sys.gpu || '—' }),
      h(Line, { key: 'ram', label: 'Memory', value: sys.ram || '—' }),
    ];
    if (s.display) perf.push(h(Line, { key: 'disp', label: 'Display', value: s.display }));
    out.push(h(Section, { key: 'perf', title: 'Performance' }, perf));

    // Batteries.
    const bats = s.batteries;
    const batKids = [];
    if (bats == null) batKids.push(h('div', { key: 'scan', className: 'uc-sub' }, 'Reading devices…'));
    else if (bats.length === 0) batKids.push(h('div', { key: 'none', className: 'uc-sub' }, 'No wireless devices with a battery.'));
    for (const b of bats || []) {
      batKids.push(
        h(
          'div',
          { key: b.name },
          h(Line, { label: b.name, value: [b.level, b.power].filter(Boolean).join(' · ') }),
          h(Bar, { fraction: b.fraction, low: b.low }),
        ),
      );
    }
    batKids.push(h(Buttons, { key: 'rescan' }, h(Btn, { wide: true, onClick: () => act('rescanBatteries') }, 'Read again')));
    out.push(h(Section, { key: 'bat', title: 'Batteries' }, batKids));

    return out;
  }

  /** Tab-strip icon: a controller. */
  function TabIcon() {
    return h(
      'svg',
      { viewBox: '0 0 24 24', width: '1em', height: '1em', fill: 'currentColor', style: { width: '100%', height: '100%' } },
      h('path', {
        fillRule: 'evenodd',
        d:
          'M7 6h10a5 5 0 0 1 4.9 6l-1 5a3 3 0 0 1-5.2 1.4L13.6 16h-3.2l-2.1 2.4A3 3 0 0 1 3.1 17l-1-5A5 5 0 0 1 7 6z' +
          'M7.25 9v1.75H5.5v1.5h1.75V14h1.5v-1.75h1.75v-1.5H8.75V9z' +
          'M16 9.25a1 1 0 1 0 0 2a1 1 0 1 0 0-2z' +
          'M18 11.75a1 1 0 1 0 0 2a1 1 0 1 0 0-2z',
      }),
    );
  }

  // ------------------------------------------------------------------ start

  const api = { version: VERSION, reply, push, health, ensure, open, closeMenus, dispose, call };
  window.__uc = api;

  ensure();
  // Big Picture's code loads later than the shared context, and Steam can
  // rebuild the menu; keep trying until hooked, then just keep the kit fresh.
  uc.timers.push(
    setInterval(() => {
      try {
        ensure();
      } catch (e) {
        problem('retry failed', e);
      }
    }, 3000),
  );
  return health();
})();
