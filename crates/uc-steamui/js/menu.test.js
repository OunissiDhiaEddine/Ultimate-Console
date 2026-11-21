/*
 * Smoke test for menu.js against fake Steams: an older build (webpack,
 * SP_REACT, React 18 elements) and a current one (one ES module, no
 * webpack, no SP_REACT, React 19 elements, reachable only through the
 * fibers on Big Picture's DOM). It checks the hooking logic, the element
 * format, the panel and the bridge, not Steam itself (that needs the real
 * PC: `uc-probe steam-menu`).
 *
 * Run: node crates/uc-steamui/js/menu.test.js
 * (or macOS JavaScriptCore: jsc menu.test.js -- menu.js)
 */
/* global require, process, readFile, print, arguments */
'use strict';

const out = typeof print === 'function' ? print : console.log;
let source;
if (typeof require === 'function') {
  const path = require('path');
  source = require('fs').readFileSync(path.join(__dirname, 'menu.js'), 'utf8');
} else {
  source = readFile(arguments[0]);
}
if (typeof setTimeout === 'undefined') globalThis.setTimeout = () => 0;
if (typeof clearTimeout === 'undefined') globalThis.clearTimeout = () => {};
const realSetInterval = globalThis.setInterval;
const intervals = [];
globalThis.setInterval = (fn) => {
  intervals.push(fn);
  return intervals.length;
};
globalThis.clearInterval = () => {};

let failures = 0;
function check(cond, what) {
  if (cond) out('ok   ' + what);
  else {
    failures++;
    out('FAIL ' + what);
  }
}

// ----------------------------------------------------------- fake Steam

const E18 = Symbol.for('react.element');
const E19 = Symbol.for('react.transitional.element');
const MEMO = Symbol.for('react.memo');

const h = (type, props, ...children) => ({
  $$typeof: E18,
  type,
  key: null,
  props: Object.assign({}, props, children.length ? { children: children.length === 1 ? children[0] : children } : {}),
});
const FakeReact = { createElement: h, Component: function Component() {} };
/** What React 19 Steam's own code would produce. */
const h19 = (type, props) => ({ $$typeof: E19, type, key: null, props: Object.assign({}, props) });

const steamTabs = () => [{ key: 1, title: 'Notifications', tab: 'bell', panel: 'list' }];
const steamTabs19 = () => [{ key: 1, title: 'Notifications', tab: h19('svg'), panel: h19('div') }];
const TabStrip = function TabStrip() {
  return null;
};

function findTabs(node) {
  if (!node || typeof node !== 'object') return null;
  if (Array.isArray(node)) {
    for (const n of node) {
      const r = findTabs(n);
      if (r) return r;
    }
    return null;
  }
  if (node.props && Array.isArray(node.props.tabs)) return node.props.tabs;
  return node.props ? findTabs(node.props.children) : null;
}
function findBy(node, pred) {
  if (!node || typeof node !== 'object') return null;
  if (Array.isArray(node)) {
    for (const n of node) {
      const r = findBy(n, pred);
      if (r) return r;
    }
    return null;
  }
  if (pred(node)) return node;
  return node.props ? findBy(node.props.children, pred) : null;
}

/** A document whose one top element carries `fiber` (like React's DOM). */
const docWith = (fiber) => ({ querySelectorAll: () => (fiber ? [{ __reactFiber$abc: fiber, parentElement: null }] : []) });
const emptyDoc = () => docWith(null);

/** Links child/return/sibling for a fiber tree given as nested specs. */
function fiberTree(spec, parent) {
  const f = Object.assign({ memoizedProps: {}, child: null, sibling: null, return: parent || null }, spec);
  const kids = (spec.kids || []).map((k) => fiberTree(k, f));
  delete f.kids;
  f.child = kids[0] || null;
  kids.forEach((k, i) => (k.sibling = kids[i + 1] || null));
  return f;
}

function install(env) {
  globalThis.window = globalThis;
  window.sent = [];
  window.ucSteamMenuSend = (json) => window.sent.push(JSON.parse(json));
  delete window.g_PopupManager;
  delete window.SteamUIStore;
  if (env.modern) {
    delete window.SP_REACT;
    delete window.webpackChunksteamui;
  } else {
    window.SP_REACT = FakeReact;
    const cache = {};
    (env.modules || []).forEach((exports, i) => (cache[i] = { exports }));
    window.webpackChunksteamui = { push: ([, , runtime]) => runtime({ c: cache }) };
  }
  globalThis.document = env.document || emptyDoc();
  if (env.popups) window.g_PopupManager = { m_mapPopups: new Map(Object.entries(env.popups).map(([k, d]) => [k, { m_popup: { document: d } }])) };
}

function inject() {
  const script = source.replace(/__UC_VERSION__/g, 'test').replace(/__UC_BINDING__/g, 'ucSteamMenuSend');
  // eslint-disable-next-line no-eval
  return (0, eval)(script);
}

/**
 * Renders down to host elements like React would: function components are
 * called, class components constructed (with an updater) and `setup` can
 * give them state. Collects the text it sees.
 */
function render(node, setup) {
  const texts = [];
  const hosts = [];
  const all = [];
  const deep = (n) => {
    if (n == null || typeof n === 'boolean') return;
    if (typeof n === 'string' || typeof n === 'number') return void texts.push(String(n));
    if (Array.isArray(n)) return n.forEach(deep);
    all.push(n);
    const t = n.type;
    if (typeof t === 'function' && t.prototype && t.prototype.isReactComponent) {
      const inst = new t(n.props || {});
      inst.updater = {
        enqueueSetState: (i, p) => Object.assign(i.state, typeof p === 'function' ? p(i.state, i.props) : p),
        enqueueForceUpdate: () => {},
      };
      if (setup) setup(inst);
      return deep(inst.render());
    }
    if (typeof t === 'function') return deep(t(n.props || {}));
    hosts.push(n);
    if (n.props) {
      for (const v of Object.values(n.props)) if (typeof v === 'string' && v.length < 80) texts.push(v);
      deep(n.props.children);
    }
  };
  deep(node);
  return { text: texts.join(' | '), hosts, all };
}

const sample = {
  host: { gaming: true, mode: 'Gaming', tv: 'Awake' },
  volume: { level: 0.45, muted: false },
  apps: [{ pid: 42, name: 'eldenring', level: 0.5, muted: true }],
  outputs: [{ id: 'a', name: 'TV', default: true }, { id: 'b', name: 'Headset', default: false }],
  tasks: [{ hwnd: 4660, pid: 42, title: 'ELDEN RING', detail: 'eldenring.exe · CPU 30%' }],
  system: { cpu: '12%', gpu: null, ram: '8.0 GB of 32.0 GB' },
  display: '3840 × 2160 · 120 Hz',
  batteries: [{ name: 'Xbox Controller', level: '15%', power: '', fraction: 0.15, low: true }],
};
const WANT = ['Leave gaming mode', 'Volume', '45%', 'eldenring (muted)', 'Headset', 'ELDEN RING', 'Switch to', '12%', '3840 × 2160 · 120 Hz', 'Xbox Controller', '15%'];

// ------------------------------------- 1: older Steam, the QAM module

{
  // Outer QAM -> inner component (onFocusNavDeactivated) -> tab strip with `tabs`.
  const Inner = function Inner() {
    return h(TabStrip, { tabs: steamTabs() });
  };
  const QamMemo = {
    $$typeof: MEMO,
    type: function QuickAccessMenu(props) {
      const marker = 'QuickAccessMenuBrowserView';
      return h('div', { className: marker }, h(Inner, { onFocusNavDeactivated() {}, visible: props.visible }));
    },
  };
  install({ modules: [{ a: QamMemo, b: { unrelated: true } }] });

  const health = inject();
  check(health && health.hooked === true, 'webpack: hooked');
  check(health.how === 'QuickAccessMenu component', 'webpack: says how');

  const renderQam = () => {
    const outer = QamMemo.type({ visible: true });
    const inner = findBy(outer, (n) => n.props && n.props.onFocusNavDeactivated);
    check(inner && inner.type !== Inner && inner.type.__ucOriginal === Inner, 'inner component wrapped');
    return findTabs(inner.type(inner.props));
  };
  const tabs = renderQam();
  check(tabs && tabs.length === 2 && tabs[1].key === 7331, "tab appended after Steam's own");
  check(tabs && tabs[1].title === 'Ultimate Console', 'tab title');
  check(tabs && tabs[1].panel && tabs[1].tab, 'tab has a panel and an icon');
  check(renderQam().length === 2, 'fresh render: still exactly one extra tab');
  check(window.__uc.health().tabShown === true, 'health reports the tab shown');

  // A memoised tabs array must not grow on every render.
  const memoTabs = steamTabs();
  const Memo = { $$typeof: MEMO, type: function QAM2() { const m = 'QuickAccessMenuBrowserView'; return h(TabStrip, { tabs: memoTabs, m }); } };
  window.__uc.dispose();
  install({ modules: [{ Memo }] });
  inject();
  Memo.type({});
  Memo.type({});
  check(memoTabs.length === 2, 'memoised tabs array gets the tab once');

  // Re-injecting the same version keeps the hook (no double wrapping).
  const before = Memo.type;
  const h2 = inject();
  check(h2.hooked && Memo.type === before, 'same version injected twice: no-op');

  // dispose() restores Steam's component.
  window.__uc.dispose();
  check(Memo.type.__ucOriginal === undefined, 'dispose restores the original component');
  check(window.__uc === undefined, 'dispose removes window.__uc');

  // The panel (class component) with Steam's React.
  const P = { $$typeof: MEMO, type: function QAM3() { const m = 'QuickAccessMenuBrowserView'; return h(TabStrip, { tabs: steamTabs(), m }); } };
  install({ modules: [{ P }] });
  inject();
  const tab = findTabs(P.type({})).find((t) => t.key === 7331);
  let r = null;
  try {
    r = render(tab.panel, (inst) => (inst.state.data = sample));
  } catch (e) {
    check(false, 'panel renders: ' + e);
  }
  if (r) for (const want of WANT) check(r.text.includes(want), 'panel shows ' + JSON.stringify(want));
  check(render(tab.panel).text.includes('Loading'), 'panel says Loading before data arrives');
  check(render(tab.tab).hosts.length > 0, 'tab icon renders');
  window.__uc.dispose();
}

// -------------------------------------------------------------- 2: bridge

{
  install({ modules: [{}] });
  inject();
  let result = null;
  let error = null;
  const p1 = window.__uc.call('state', {}).then((r) => (result = r));
  const p2 = window.__uc.call('setVolume', { level: 0.5 }).catch((e) => (error = e.message));
  const [m1, m2] = window.sent.filter((m) => m.method !== 'log');
  check(m1 && m1.method === 'state' && m1.id > 0, 'call sends {id, method}');
  check(m2 && m2.params.level === 0.5, 'call sends params');
  window.__uc.reply({ id: m1.id, result: { mode: 'Gaming' } });
  window.__uc.reply({ id: m2.id, error: 'no audio' });
  window.__uc.reply({ id: 9999, result: 1 }); // unknown id: ignored
  Promise.all([p1, p2]).then(() => {
    check(result && result.mode === 'Gaming', 'reply resolves the call');
    check(error === 'no audio', 'error reply rejects the call');
    window.__uc.dispose();
    fiberTests();
  });
}

function fiberTests() {
  // ------------------------- 3: older Steam, only the tabs holder mounted

  {
    const Owner = function Owner() {
      return h('div', null, h(TabStrip, { tabs: steamTabs() }));
    };
    const OwnerMemo = { $$typeof: MEMO, type: Owner };
    const root = fiberTree({
      type: null,
      kids: [{ elementType: OwnerMemo, type: Owner, kids: [{ type: 'div', kids: [{ type: TabStrip, memoizedProps: { tabs: steamTabs() } }] }] }],
    });
    const owner = root.child;
    const alt = { type: Owner };
    owner.alternate = alt;
    install({ modules: [{}], document: docWith(owner.child) });
    const health = inject();
    check(health.hooked && health.how === 'mounted Quick Access Menu', 'tabs holder: hooked');
    check(owner.type !== Owner && owner.type.__ucOriginal === Owner, 'mounted fiber points at the wrapper');
    check(alt.type === owner.type, 'alternate fiber too');
    check(OwnerMemo.type === owner.type, 'memo patched for future mounts');
    const tabs = findTabs(owner.type({}));
    check(tabs && tabs.some((t) => t.key === 7331), 'tabs holder: tab added on render');
    window.__uc.dispose();
  }

  // ------------- 4: current Steam: no webpack, no SP_REACT, React 19, popups

  {
    const QamMemo = {
      $$typeof: MEMO,
      type: function QAM() {
        const m = 'QuickAccessMenuBrowserView';
        return h19(TabStrip, { tabs: steamTabs19(), m });
      },
    };
    // As in current Steam: a plain function that takes its props apart.
    const Focusable = function Rh(e) {
      const { "flow-children": t, onActivate: n, onCancel: r, focusClassName: i, focusWithinClassName: a, ...o } = e;
      void [t, n, r, i, a];
      return h19('div', Object.assign({}, o));
    };
    // Passes `flow-children` on but isn't Focusable: must not be picked.
    const Dialog = function Ta(p) {
      return h19('div', { "flow-children": 'row', onActivate: p.onActivate, focusClassName: p.focusClassName });
    };
    const root = fiberTree({
      type: null,
      kids: [
        {
          elementType: QamMemo,
          type: QamMemo.type,
          kids: [{ type: 'div', memoizedProps: { children: h19('span') }, kids: [{ type: Dialog }, { type: Focusable }] }],
        },
      ],
    });
    const qamFiber = root.child;
    let opened = null;
    install({ modern: true, popups: { 'SP Desktop_uid0': emptyDoc(), 'SP BPM_uid0': docWith(qamFiber.child), contextmenu_1_uid0: emptyDoc() } });
    window.SteamUIStore = { WindowStore: { GamepadUIMainWindowInstance: { MenuStore: { OpenQuickAccessMenu: (k) => (opened = k), CloseSideMenus() {} } } } };

    const health = inject();
    check(health.webpack === false, 'modern: no webpack');
    check(health.hooked === true && health.how === 'QuickAccessMenu component', 'modern: hooked through the fibers');
    check(health.diag.documents.includes('SP BPM_uid0') && !health.diag.documents.includes('contextmenu_1_uid0'), 'modern: looks in Big Picture, skips context menus');
    check(health.diag.element === 'react.transitional.element', 'modern: learned the React 19 element format');
    check(health.found.Focusable === true, "modern: found Steam's Focusable on screen (a plain function)");
    check(health.found.MenuStore === true, 'modern: found the menu store');
    check(QamMemo.type.__ucOriginal && qamFiber.type === QamMemo.type, 'modern: memo and mounted fiber wrapped');

    const tabs = findTabs(QamMemo.type({}));
    const tab = tabs && tabs.find((t) => t.key === 7331);
    check(!!tab, 'modern: tab added');
    check(tab && tab.panel.$$typeof === E19 && tab.tab.$$typeof === E19, "modern: elements use Steam's format");
    let r = null;
    try {
      r = render(tab.panel, (inst) => (inst.state.data = sample));
    } catch (e) {
      check(false, 'modern: panel renders: ' + e);
    }
    if (r) {
      for (const want of ['Leave gaming mode', 'ELDEN RING', 'Xbox Controller']) check(r.text.includes(want), 'modern: panel shows ' + JSON.stringify(want));
      check(r.hosts.every((n) => n.$$typeof === E19), 'modern: every element in the panel uses the React 19 format');
      const rootDiv = r.hosts.find((n) => n.props && n.props.className === 'uc-root');
      check(rootDiv && typeof rootDiv.props.ref === 'function', 'modern: ref passed as a prop (React 19)');
      const rows = r.all.filter((n) => n.type === Focusable && n.props['flow-children']).map((n) => n.props['flow-children']);
      check(rows.length > 0 && rows.every((v) => v === 'row'), "modern: button rows use Steam's Focusable with flow-children row");
    }
    check(window.__uc.open() === true && opened === 7331, 'modern: open() opens the menu on our tab');
    window.__uc.dispose();
    check(qamFiber.type.__ucOriginal === undefined, 'modern: dispose restores the fiber');
  }

  // --------------------------- 5: nothing there yet (Steam still loading)

  {
    install({ modern: true });
    const early = inject();
    check(early.webpack === false && early.hooked === false, "before Steam's UI loads: waits");
    check(intervals.length > 0, 'a retry timer is running');
    window.__uc.dispose();
  }

  if (realSetInterval) globalThis.setInterval = realSetInterval;
  out(failures === 0 ? 'ALL OK' : failures + ' FAILED');
  if (typeof process !== 'undefined' && failures) process.exitCode = 1;
}
