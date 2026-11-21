/*
 * Smoke test for menu.js against a fake Steam: a stand-in React, webpack
 * module cache, Quick Access Menu and React fiber tree. It checks the
 * hooking logic and the bridge, not Steam itself (that needs the real PC:
 * `uc-probe steam-menu`).
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

const h = (type, props, ...children) => ({
  $$typeof: Symbol.for('react.element'),
  type,
  props: Object.assign({}, props, children.length ? { children: children.length === 1 ? children[0] : children } : {}),
});
const FakeReact = { createElement: h, useState: () => [null, () => {}], Component: function () {} };

const steamTabs = () => [{ key: 1, title: 'Notifications', tab: 'bell', panel: 'list' }];
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

function install(env) {
  globalThis.window = globalThis;
  window.SP_REACT = FakeReact;
  window.sent = [];
  window.ucSteamMenuSend = (json) => window.sent.push(JSON.parse(json));
  const cache = {};
  env.modules.forEach((exports, i) => (cache[i] = { exports }));
  window.webpackChunksteamui = { push: ([, , runtime]) => runtime({ c: cache }) };
  globalThis.document = env.document || { getElementById: () => null, body: null };
  delete window.g_PopupManager;
}

function inject() {
  const script = source.replace(/__UC_VERSION__/g, 'test').replace(/__UC_BINDING__/g, 'ucSteamMenuSend');
  // eslint-disable-next-line no-eval
  return (0, eval)(script);
}

// ------------------------------------------- 1: the QAM component module

{
  // Outer QAM -> inner component (onFocusNavDeactivated) -> tab strip with `tabs`.
  const Inner = function Inner() {
    return h(TabStrip, { tabs: steamTabs() });
  };
  const QamMemo = {
    $$typeof: Symbol.for('react.memo'),
    type: function QuickAccessMenu(props) {
      const marker = 'QuickAccessMenuBrowserView';
      return h('div', { className: marker }, h(Inner, { onFocusNavDeactivated() {}, visible: props.visible }));
    },
  };
  install({ modules: [{ a: QamMemo, b: { unrelated: true } }] });

  const health = inject();
  check(health && health.hooked === true, 'module strategy: hooked');
  check(health.how === 'QuickAccessMenu component', 'module strategy: says how');

  const render = () => {
    const outer = QamMemo.type({ visible: true });
    const inner = findBy(outer, (n) => n.props && n.props.onFocusNavDeactivated);
    check(inner && inner.type !== Inner && inner.type.__ucOriginal === Inner, 'inner component wrapped');
    return findTabs(inner.type(inner.props));
  };
  const tabs = render();
  check(tabs && tabs.length === 2 && tabs[1].key === 7331, 'tab appended after Steam\'s own');
  check(tabs && tabs[1].title === 'Ultimate Console', 'tab title');
  check(tabs && tabs[1].panel && tabs[1].tab, 'tab has a panel and an icon');
  const again = render();
  check(again.length === 2, 'fresh render: still exactly one extra tab');
  check(window.__uc.health().tabShown === true, 'health reports the tab shown');

  // A memoised tabs array must not grow on every render.
  const memoTabs = steamTabs();
  const Memo = { $$typeof: Symbol.for('react.memo'), type: function QAM2() { const m = 'QuickAccessMenuBrowserView'; return h(TabStrip, { tabs: memoTabs, m }); } };
  install({ modules: [{ Memo }] });
  window.__uc.dispose();
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
    fiberTest();
  });
}

// -------------------------------------- 4: the panel renders sample data

function panelTest() {
  const states = [];
  FakeReact.useState = (init) => [states.length ? states.shift() : init, () => {}];
  FakeReact.useRef = (init) => ({ current: init });
  FakeReact.useEffect = () => {};
  FakeReact.useCallback = (fn) => fn;
  const QamMemo = {
    $$typeof: Symbol.for('react.memo'),
    type: function QAM() { const m = 'QuickAccessMenuBrowserView'; return h(TabStrip, { tabs: steamTabs(), m }); },
  };
  install({ modules: [{ QamMemo }] });
  inject();
  const tab = findTabs(QamMemo.type({})).find((t) => t.key === 7331);

  // Renders every function component down to plain elements; collects text.
  const texts = [];
  const deep = (node) => {
    if (node == null || typeof node === 'boolean') return;
    if (typeof node === 'string' || typeof node === 'number') return void texts.push(String(node));
    if (Array.isArray(node)) return node.forEach(deep);
    if (typeof node.type === 'function') return deep(node.type(node.props || {}));
    if (node.props) {
      for (const v of Object.values(node.props)) if (typeof v === 'string' && v.length < 80) texts.push(v);
      deep(node.props.children);
    }
  };
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
  states.push(sample);
  let threw = null;
  try {
    deep(tab.panel);
  } catch (e) {
    threw = e;
  }
  check(!threw, 'panel renders with fallback controls' + (threw ? ': ' + threw : ''));
  const all = texts.join(' | ');
  for (const want of ['Leave gaming mode', 'Volume', '45%', 'eldenring (muted)', 'Headset', 'ELDEN RING', 'Switch to', '12%', '3840 × 2160 · 120 Hz', 'Xbox Controller', '15%']) {
    check(all.includes(want), 'panel shows ' + JSON.stringify(want));
  }
  // Loading state before the first answer.
  texts.length = 0;
  deep(tab.panel);
  check(texts.join(' ').includes('Loading'), 'panel says Loading before data arrives');
  // Tab icon renders.
  texts.length = 0;
  deep(tab.tab);
  check(texts.length > 0, 'tab icon renders');
  window.__uc.dispose();
}

// ------------------------------------------- 3: fallback through fibers

function fiberTest() {
  // No QuickAccessMenuBrowserView anywhere; a mounted owner renders the tabs.
  const Owner = function Owner() {
    return h('div', null, h(TabStrip, { tabs: steamTabs() }));
  };
  const OwnerMemo = { $$typeof: Symbol.for('react.memo'), type: Owner };
  const strip = { type: TabStrip, memoizedProps: { tabs: steamTabs() }, child: null, sibling: null };
  const div = { type: 'div', memoizedProps: {}, child: strip, sibling: null };
  const owner = { type: Owner, memoizedProps: {}, child: div, sibling: null };
  const alt = { type: Owner };
  owner.alternate = alt;
  const root = { type: null, child: owner, sibling: null, return: null };
  strip.return = div;
  div.return = owner;
  owner.return = root;
  const container = { __reactContainer$abc: root };
  install({
    modules: [{ OwnerMemo }],
    document: { getElementById: (id) => (id === 'root' ? container : null), body: null },
  });
  const health = inject();
  check(health.hooked && health.how === 'mounted Quick Access Menu', 'fiber strategy: hooked');
  check(owner.type !== Owner && owner.type.__ucOriginal === Owner, 'mounted fiber points at the wrapper');
  check(alt.type === owner.type, 'alternate fiber too');
  check(OwnerMemo.type === owner.type, 'memo export patched for future mounts');
  const tabs = findTabs(owner.type({}));
  check(tabs && tabs.some((t) => t.key === 7331), 'fiber strategy: tab added on render');

  // Nothing to hook: stays unhooked, reports it, keeps retrying.
  window.__uc.dispose();
  install({ modules: [{}] });
  const none = inject();
  check(none.hooked === false && none.webpack === true, 'nothing to hook: not hooked, still retrying');
  check(intervals.length > 0, 'a retry timer is running');
  window.__uc.dispose();

  // Webpack not there yet (Steam still loading).
  install({ modules: [] });
  delete window.webpackChunksteamui;
  const early = inject();
  check(early.webpack === false && early.hooked === false, 'before Steam\'s UI loads: waits');
  window.__uc.dispose();

  panelTest();

  if (realSetInterval) globalThis.setInterval = realSetInterval;
  out(failures === 0 ? 'ALL OK' : failures + ' FAILED');
  if (typeof process !== 'undefined' && failures) process.exitCode = 1;
}
