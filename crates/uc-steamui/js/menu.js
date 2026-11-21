/*
 * Ultimate Console tab for Steam's Quick Access Menu (Steam + A in Big
 * Picture and in the in-game overlay).
 *
 * uc-agent evaluates this file in Steam's "SharedJSContext" over the CEF
 * DevTools port (see crates/uc-steamui). Plain JavaScript, no build step.
 * `__UC_VERSION__` and `__UC_BINDING__` are filled in by the agent.
 *
 * How it hooks in. Steam's UI is a React app. Older builds were bundled with
 * webpack and exposed `webpackChunksteamui` and `SP_REACT`; current builds
 * load as one ES module (`library.js`) and expose neither. So the script
 * works from what is always there: the React fibers React attaches to every
 * rendered DOM node (`__reactFiber$…`), in this window and in each Steam
 * popup (`g_PopupManager`: the desktop client, Big Picture, overlays).
 *
 *  1. Walk the fiber trees. Every component type seen is a candidate for
 *     Steam's building blocks (Focusable, SliderField …, found by marker
 *     strings in their source) and for the Quick Access Menu itself.
 *  2. The Quick Access Menu is the component whose source mentions
 *     `QuickAccessMenuBrowserView`, or failing that whichever component
 *     renders a `tabs` array of QAM-like tabs ({key, tab, panel}). We wrap
 *     its render function (patching the memo object, so future mounts too,
 *     and the mounted fibers); each render appends our tab to that array.
 *  3. Elements are made without Steam's React: plain element objects whose
 *     `$$typeof` is copied from Steam's own tabs (React 18 and 19 differ).
 *     The panel is a class component, which needs no hooks: React gives it
 *     `this.updater` for setState.
 *  4. With webpack (older Steam) the same things are also looked up in the
 *     module cache, and Steam's React is used when it can be found.
 *
 * Everything Steam-internal is looked up defensively. `window.__uc.health()`
 * reports what was found; the agent logs it and `uc-probe steam-menu run`
 * prints it.
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
    diag: { documents: [], roots: 0, fibers: 0, types: 0, element: null },
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

  // ------------------------------------------------- webpack (older Steam)

  let wpRequire = null;
  let moduleCount = 0;
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

  /** Every export of every loaded webpack module; empty without webpack. */
  function allExports() {
    const req = webpackRequire();
    if (!req || !req.c) return exportsCache;
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

  function findIn(list, pred) {
    for (const e of list) {
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
  const sourceCache = new WeakMap();
  function componentSource(c, depth) {
    if (!c || (depth || 0) > 3) return '';
    if (typeof c !== 'function' && typeof c !== 'object') return '';
    if (sourceCache.has(c)) return sourceCache.get(c);
    let s = '';
    if (typeof c === 'function') s = fnSource(c);
    else s = fnSource(c.render) || componentSource(c.type, (depth || 0) + 1);
    sourceCache.set(c, s);
    return s;
  }

  // ---------------------------------------------------------------- fibers

  /** Every document Steam's UI renders into: this one and its popups. */
  function documents() {
    const docs = [{ name: 'SharedJSContext', doc: document }];
    try {
      const pm = window.g_PopupManager;
      let entries = [];
      if (pm && pm.m_mapPopups && typeof pm.m_mapPopups.entries === 'function') entries = Array.from(pm.m_mapPopups.entries());
      else if (pm && typeof pm.GetPopups === 'function') entries = Array.from(pm.GetPopups() || []).map((p, i) => [String(i), p]);
      for (const [name, p] of entries) {
        if (/^contextmenu_/.test(name)) continue;
        try {
          const w = p && (p.m_popup || p.window || p.m_window);
          if (w && w.document && w.document !== document) docs.push({ name, doc: w.document });
        } catch (e) {
          /* closed popup */
        }
      }
    } catch (e) {
      /* no popup manager */
    }
    uc.diag.documents = docs.map((d) => d.name);
    return docs;
  }

  /** The React roots (HostRoot fibers) of every document. */
  function rootFibers() {
    const roots = new Set();
    for (const { doc } of documents()) {
      let els;
      try {
        els = doc.querySelectorAll ? doc.querySelectorAll('*') : [];
      } catch (e) {
        continue;
      }
      let fiberKey = null;
      const limit = Math.min(els.length, 20000);
      for (let i = 0; i < limit; i++) {
        const el = els[i];
        if (!fiberKey) {
          fiberKey = Object.keys(el).find((k) => k.startsWith('__reactFiber$') || k.startsWith('__reactContainer$')) || null;
          if (!fiberKey) continue;
          fiberKey = fiberKey.startsWith('__reactContainer$') ? '__reactFiber$' + fiberKey.slice('__reactContainer$'.length) : fiberKey;
        }
        const own = el[fiberKey];
        // Only climb from the top element of each React tree.
        if (!own || (el.parentElement && el.parentElement[fiberKey])) continue;
        let f = own;
        let n = 0;
        while (f.return && n++ < 10000) f = f.return;
        roots.add(f);
      }
    }
    uc.diag.roots = roots.size;
    return Array.from(roots);
  }

  function walkFibers(visit) {
    const stack = rootFibers();
    const seen = new Set();
    let n = 0;
    while (stack.length && n < 500000) {
      const f = stack.pop();
      if (!f || seen.has(f)) continue;
      seen.add(f);
      n++;
      if (visit(f) === false) break;
      if (f.sibling) stack.push(f.sibling);
      if (f.child) stack.push(f.child);
    }
    return n;
  }

  const isQamTab = (t) => t && typeof t === 'object' && 'key' in t && ('panel' in t || 'tab' in t);
  const holdsTabs = (n) => n && n.props && Array.isArray(n.props.tabs) && n.props.tabs.some(isQamTab);

  // ----------------------------------------------------------------- React

  /** Steam's React, when it can be reached (older builds). */
  let realReact = null;
  function findReact() {
    if (realReact) return realReact;
    const r = window.SP_REACT;
    if (r && r.createElement && r.Component) realReact = r;
    else realReact = findIn(allExports(), (e) => e.createElement && e.Component && e.useState);
    return realReact;
  }

  /** `$$typeof` of React elements in this Steam, copied from Steam's own. */
  let ELEMENT = null;
  function learnElement(obj, depth) {
    if (ELEMENT || !obj || typeof obj !== 'object' || (depth || 0) > 3) return;
    if (Array.isArray(obj)) {
      for (const o of obj.slice(0, 10)) learnElement(o, (depth || 0) + 1);
      return;
    }
    const t = obj.$$typeof;
    if (typeof t === 'symbol') {
      const d = t.description || String(t);
      if (/^react\.(transitional\.)?element$/.test(d)) {
        ELEMENT = t;
        uc.diag.element = d;
      }
    }
  }
  const isReact19 = () => !!(ELEMENT && /transitional/.test(ELEMENT.description || ''));

  /** createElement, with Steam's React or by hand. */
  function h(type, props, ...children) {
    const R = findReact();
    if (R) return R.createElement(type, props, ...children);
    if (!ELEMENT) throw new Error('React element format not known yet');
    const p = {};
    let key = null;
    let ref = null;
    if (props) {
      for (const k of Object.keys(props)) {
        if (k === 'key') key = props[k] == null ? null : String(props[k]);
        else if (k === 'ref') ref = props[k];
        else p[k] = props[k];
      }
    }
    if (children.length === 1) p.children = children[0];
    else if (children.length > 1) p.children = children;
    if (type && type.defaultProps) {
      for (const k of Object.keys(type.defaultProps)) if (p[k] === undefined) p[k] = type.defaultProps[k];
    }
    if (ref != null && isReact19()) p.ref = ref;
    return { $$typeof: ELEMENT, type, key, ref, props: p, _owner: null, _store: {} };
  }

  /**
   * Base for class components that works without Steam's React: React only
   * needs `prototype.isReactComponent`, and hands every instance an
   * `updater` for setState.
   */
  function Base(props) {
    this.props = props;
    this.state = {};
  }
  Base.prototype.isReactComponent = {};
  Base.prototype.setState = function setState(partial, callback) {
    if (this.updater) this.updater.enqueueSetState(this, partial, callback, 'setState');
  };
  Base.prototype.forceUpdate = function forceUpdate(callback) {
    if (this.updater) this.updater.enqueueForceUpdate(this, callback, 'forceUpdate');
  };

  // ------------------------------------------------------ Steam components

  /** Steam's UI pieces, found by marker strings in their source. */
  const K = {};
  const looks = (name) => (e) => {
    const s = componentSource(e);
    return s.includes(name + ',fallback') || (s.includes('"' + name + '"') && s.includes('fallback'));
  };
  const KIT = {
    // A forwardRef object in React 18 builds, a plain function in React 19
    // ones (`function Rh(e){let{"flow-children":t,onActivate:n,…,
    // focusClassName:i,…}=e`). It takes its props apart first thing, which
    // tells it apart from components that only pass `flow-children` on.
    Focusable: (e) => {
      const s = componentSource(e);
      if (!s.includes('"flow-children"') || !s.includes('onActivate') || !s.includes('focusClassName')) return false;
      if (typeof e === 'object') return true;
      return /\{\s*"flow-children"\s*:\s*[A-Za-z_$][\w$]*\s*,\s*onActivate\s*:\s*[A-Za-z_$]/.test(s.slice(0, 600));
    },
    SliderField: looks('SliderField'),
    ToggleField: looks('ToggleField'),
    ButtonItem: (e) => looks('ButtonField')(e) || componentSource(e).includes('childrenContainerWidth:"min"'),
    Field: (e) => {
      const s = componentSource(e);
      return s.includes('"shift-children-below"') || /[^A-Za-z]Field,fallback/.test(s);
    },
  };

  function resolveKit(candidates) {
    for (const [name, pred] of Object.entries(KIT)) {
      if (!K[name]) K[name] = findIn(candidates, pred);
      uc.found[name] = !!K[name];
    }
  }

  /** The object with OpenQuickAccessMenu / CloseSideMenus, if reachable. */
  function findMenuStore() {
    const isStore = (ms) => ms && typeof ms.OpenQuickAccessMenu === 'function';
    const fromInstance = (inst) => inst && (inst.MenuStore || inst.m_MenuStore);
    const stores = [window.SteamUIStore];
    const router = findIn(allExports(), (e) => e.WindowStore && (e.Navigate || e.NavigationManager));
    if (router) stores.push(router);
    for (const s of stores) {
      try {
        const ws = s && (s.WindowStore || s.m_WindowStore);
        if (!ws) continue;
        for (const inst of [ws.GamepadUIMainWindowInstance, ws.MainWindowInstance]) {
          if (isStore(fromInstance(inst))) return fromInstance(inst);
        }
        for (const mapName of ['m_mapDesiredWindowInstances', 'm_mapAppWindows']) {
          const m = ws[mapName];
          if (!m || typeof m.values !== 'function') continue;
          for (const v of m.values()) {
            for (const inst of Array.isArray(v) ? v : [v]) {
              if (isStore(fromInstance(inst))) return fromInstance(inst);
            }
          }
        }
      } catch (e) {
        /* keep looking */
      }
    }
    return null;
  }

  // ------------------------------------------------------------ the hook

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

  /** Puts our tab into the `tabs` prop found in `tree`. */
  function addTab(tree, visible) {
    const holder = findInTree(tree, holdsTabs);
    if (!holder) return false;
    const tabs = holder.props.tabs;
    for (const t of tabs) {
      learnElement(t.panel);
      learnElement(t.tab);
      learnElement(t.title);
    }
    if (!findReact() && !ELEMENT) {
      problem("can't tell how this Steam builds React elements; tab not added");
      return false;
    }
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

  /**
   * Wraps a Quick Access Menu component: a memo or forwardRef object is
   * patched in place (covers future mounts), and mounted fibers are switched.
   */
  function patchComponent(t) {
    if (t && typeof t === 'object' && t.$$typeof && typeof t.type === 'function') {
      const orig = t.type.__ucOriginal || t.type;
      const wrapped = levels[0](orig);
      if (t.type !== wrapped) {
        t.type = wrapped;
        uc.undo.push(() => {
          if (t.type === wrapped) t.type = orig;
        });
      }
      swapFiberTypes(orig, wrapped);
      return true;
    }
    if (t && typeof t === 'object' && t.$$typeof && typeof t.render === 'function') {
      const orig = t.render.__ucOriginal || t.render;
      const wrapped = levels[0](orig);
      if (t.render !== wrapped) {
        t.render = wrapped;
        uc.undo.push(() => {
          if (t.render === wrapped) t.render = orig;
        });
      }
      return true;
    }
    if (typeof t === 'function') {
      const wrapped = levels[0](t);
      if (wrapped === t) return false;
      return swapFiberTypes(t, wrapped) > 0;
    }
    return false;
  }

  /** Component types seen in the fiber trees, and what each scan turned up. */
  const fiberTypes = [];
  const typeSeen = new WeakSet();
  let qamType = null;

  function scanFibers() {
    let holderFiber = null;
    uc.diag.fibers = walkFibers((f) => {
      const p = f.memoizedProps;
      if (p && typeof p === 'object') {
        if (!ELEMENT) learnElement(p.children);
        if (!holderFiber && Array.isArray(p.tabs) && p.tabs.some(isQamTab)) holderFiber = f;
      }
      for (const t of [f.elementType, f.type]) {
        if (!t || (typeof t !== 'function' && typeof t !== 'object') || typeSeen.has(t)) continue;
        typeSeen.add(t);
        fiberTypes.push(t);
        if (!qamType && componentSource(t).includes('QuickAccessMenuBrowserView')) qamType = t;
      }
    });
    uc.diag.types = fiberTypes.length;
    return holderFiber;
  }

  /** Strategy for the tabs holder: wrap the component that renders it. */
  function patchHolderOwner(holderFiber) {
    let owner = holderFiber.return;
    while (owner && !(typeof owner.type === 'function' || (owner.type && typeof owner.type.render === 'function'))) {
      owner = owner.return;
    }
    if (!owner) return false;
    // Prefer the memo / forwardRef object (elementType) so future mounts are covered.
    const et = owner.elementType;
    if (et && typeof et === 'object' && (et.type === owner.type || et.render)) return patchComponent(et);
    return patchComponent(owner.type);
  }

  let hookedBy = null;
  let kitCount = -1;
  let tries = 0;
  /** Tries to hook the Quick Access Menu; cheap once it has worked. */
  function ensure() {
    tries++;
    const exportsList = allExports();
    // The fiber walk is the heavy part: every other call until hooked, then
    // only now and then to pick up Steam components that weren't mounted yet.
    const missingKit = Object.keys(KIT).some((k) => !K[k]);
    let holderFiber = null;
    if (!hookedBy ? tries % 2 === 1 : missingKit && tries % 20 === 0) holderFiber = scanFibers();
    if (kitCount !== exportsList.length + fiberTypes.length) {
      kitCount = exportsList.length + fiberTypes.length;
      resolveKit(exportsList.concat(fiberTypes));
    }
    if (!K.menuStore) K.menuStore = findMenuStore();
    uc.found.MenuStore = !!K.menuStore;
    if (hookedBy) return true;
    try {
      const fromModule = findIn(
        exportsList,
        (e) => typeof e === 'object' && e.$$typeof && typeof e.type === 'function' && fnSource(e.type).includes('QuickAccessMenuBrowserView'),
      );
      if (fromModule && patchComponent(fromModule)) {
        hookedBy = 'module';
        uc.how = 'QuickAccessMenu component';
      } else if (qamType && patchComponent(qamType)) {
        hookedBy = 'fiber';
        uc.how = 'QuickAccessMenu component';
      } else if (holderFiber && patchHolderOwner(holderFiber)) {
        hookedBy = 'fiber';
        uc.how = 'mounted Quick Access Menu';
      }
    } catch (e) {
      problem('hooking the Quick Access Menu failed', e);
    }
    return !!hookedBy;
  }

  function health() {
    return {
      version: VERSION,
      webpack: !!wpRequire,
      react: !!realReact || !!ELEMENT,
      hooked: !!hookedBy,
      how: uc.how,
      tabShown: uc.patched,
      lastTabRender: uc.lastTabRender,
      found: Object.assign({}, uc.found),
      problems: uc.problems.slice(),
      diag: Object.assign({}, uc.diag),
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
    if (K.Focusable) return h(K.Focusable, { className: 'uc-row', 'flow-children': 'row' }, children);
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
   * The tab's content. A class component: it needs no hooks, so it works
   * without access to Steam's React.
   */
  class Panel extends Base {
    constructor(props) {
      super(props);
      this.state = { data: null, error: null, armed: null };
      /** Values the user just set, kept for a moment so a stale poll doesn't undo them. */
      this.overrides = new Map();
      /** Slider calls in flight, one per slider. */
      this.throttled = new Map();
      this.alive = false;
      this.root = null;
      this.setRoot = (el) => {
        this.root = el;
      };
      this.onPush = (name) => {
        if (name === 'refresh') this.refresh();
      };
    }

    componentDidMount() {
      this.alive = true;
      listeners.add(this.onPush);
      this.tick();
    }

    componentWillUnmount() {
      this.alive = false;
      clearTimeout(this.timer);
      clearTimeout(this.armTimer);
      listeners.delete(this.onPush);
    }

    /** On screen right now (the menu is open on this tab)? */
    shown() {
      const el = this.root;
      if (!el) return true;
      if (!el.isConnected) return false;
      const doc = el.ownerDocument;
      return el.getClientRects().length > 0 && (!doc || doc.visibilityState !== 'hidden');
    }

    async tick() {
      if (!this.alive) return;
      if (this.shown()) await this.refresh();
      if (this.alive) this.timer = setTimeout(() => this.tick(), 1000);
    }

    async refresh() {
      try {
        const data = await call('state');
        if (this.alive) this.setState({ data, error: null });
      } catch (e) {
        if (this.alive) this.setState({ error: e.message || String(e) });
      }
    }

    override(key, fallback) {
      const o = this.overrides.get(key);
      return o && Date.now() - o.at < 1500 ? o.value : fallback;
    }

    setOverride(key, value) {
      this.overrides.set(key, { value, at: Date.now() });
      this.forceUpdate();
    }

    /** Two presses within 3 s: the first arms, the second does it. */
    confirm(key, action) {
      clearTimeout(this.armTimer);
      if (this.state.armed === key) {
        this.setState({ armed: null });
        action();
      } else {
        this.setState({ armed: key });
        this.armTimer = setTimeout(() => this.alive && this.setState({ armed: null }), 3000);
      }
    }

    act(method, params, after) {
      return call(method, params)
        .then(() => {
          if (after) after();
          return this.refresh();
        })
        .catch((e) => this.alive && this.setState({ error: e.message || String(e) }));
    }

    /** Sliders send at most one change per 80 ms, always the latest. */
    sendThrottled(key, method, params) {
      const entry = this.throttled.get(key) || { busy: false, next: null };
      this.throttled.set(key, entry);
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
          .catch((e) => this.alive && this.setState({ error: e.message || String(e) }))
          .finally(() => setTimeout(pump, 80));
      };
      pump();
    }

    render() {
      try {
        const { data, error } = this.state;
        const kids = [h('style', { key: 'css' }, CSS)];
        if (error && !data) {
          kids.push(h('div', { key: 'err', className: 'uc-error' }, `Can't reach Ultimate Console: ${error}`));
        } else if (!data) {
          kids.push(h('div', { key: 'wait', className: 'uc-sub', style: { padding: 16 } }, 'Loading…'));
        } else {
          kids.push(...sections(data, this));
          if (error) kids.push(h('div', { key: 'err2', className: 'uc-error' }, error));
        }
        return h('div', { className: 'uc-root', ref: this.setRoot }, kids);
      } catch (e) {
        problem('the tab failed to draw', e);
        return h('div', { className: 'uc-error' }, `Ultimate Console: ${e.message || e}`);
      }
    }
  }

  function sections(s, panel) {
    const out = [];
    const host = s.host || {};
    const armed = panel.state.armed;
    const ov = (key, fallback) => panel.override(key, fallback);

    // Console: mode, TV and leaving gaming mode.
    const consoleKids = [h(Line, { key: 'mode', label: 'Mode', value: host.mode || 'Unknown' })];
    if (host.tv) consoleKids.push(h(Line, { key: 'tv', label: 'TV', value: host.tv }));
    if (host.gaming) {
      consoleKids.push(
        h(Action, {
          key: 'leave',
          button: armed === 'leave' ? 'Press again to leave' : 'Leave gaming mode',
          description: 'Back to the desktop: monitors, Explorer and the TV as they were.',
          onClick: () => panel.confirm('leave', () => panel.act('leaveGaming')),
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
          value: ov('master', s.volume.level),
          onChange: (v) => {
            panel.setOverride('master', v);
            panel.sendThrottled('master', 'setVolume', { level: v });
          },
        }),
        h(Toggle, {
          key: 'mute',
          label: 'Mute',
          checked: ov('mute', s.volume.muted),
          onChange: (m) => {
            panel.setOverride('mute', m);
            panel.act('setMuted', { muted: m });
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
          value: ov(key, app.level),
          onChange: (v) => {
            panel.setOverride(key, v);
            panel.sendThrottled(key, 'setAppVolume', { pid: app.pid, level: v });
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
              onClick: () => panel.act('setOutput', { id: d.id }),
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
                  h(Btn, { wide: true, onClick: () => panel.act('switchTo', { hwnd: t.hwnd }, closeMenus) }, 'Switch to'),
                  h(Btn, { onClick: () => panel.act('closeWindow', { hwnd: t.hwnd }) }, 'Close'),
                  h(
                    Btn,
                    {
                      danger: armed === 'kill' + t.pid,
                      onClick: () => panel.confirm('kill' + t.pid, () => panel.act('endProcess', { pid: t.pid })),
                    },
                    armed === 'kill' + t.pid ? 'Sure?' : 'End',
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
    batKids.push(h(Buttons, { key: 'rescan' }, h(Btn, { wide: true, onClick: () => panel.act('rescanBatteries') }, 'Read again')));
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

  try {
    ensure();
  } catch (e) {
    problem('first look failed', e);
  }
  // Big Picture opens later than the shared context, and Steam can rebuild
  // the menu; keep trying until hooked, then just keep the kit fresh.
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
