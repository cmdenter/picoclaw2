import { picoclaw } from "declarations/picoclaw";
import { AuthClient } from "@dfinity/auth-client";
import { HttpAgent, Actor } from "@dfinity/agent";

// ── Cached DOM (single lookup at init, never re-queried) ────────────
const chatArea  = document.getElementById('chatArea');
const inputEl   = document.getElementById('input');
const sendBtn   = document.getElementById('sendBtn');
const typingEl  = document.getElementById('typing');
const statusDot = document.getElementById('statusDot');
const authBar   = document.getElementById('authBar');
const toastEl   = document.getElementById('toast');
const statMsgs  = document.getElementById('statMsgs');
const statCalls = document.getElementById('statCalls');
const statCycles= document.getElementById('statCycles');
const statQueue = document.getElementById('statQueue');
const statSpent = document.getElementById('statSpent');
const statCost  = document.getElementById('statCost');
const stopBtn   = document.getElementById('stopBtn');
const memPanel  = document.getElementById('memPanel');
const memToggle = document.getElementById('memToggleBtn');
const memStatus = document.getElementById('memStatus');
const memTs     = document.getElementById('memTs');
const webMemBody  = document.getElementById('webMemBody');
const webMemCount = document.getElementById('webMemCount');

// ── State ────────────────────────────────────────────────────────────
let actor = picoclaw;
let authClient = null;
let identity = null;
let principalId = null;
let authProvider = null;
let memPanelOpen = false;
let toastTimer = 0;

// ── Message queue (never blocks the send button) ─────────────────────
const queue = [];
let processing = false;
let stopped = false;

// ── Tiny helpers (inlined on hot path) ──────────────────────────────
function syncSend() {
  sendBtn.disabled = !identity;
}

function scroll() {
  chatArea.scrollTop = chatArea.scrollHeight;
}

function appendMsg(role, text, ts) {
  const d = document.createElement('div');
  d.className = 'msg ' + role;
  d.textContent = text || '';
  if (ts) {
    const s = document.createElement('span');
    s.className = 'ts';
    s.textContent = new Date(Number(ts) / 1e6).toLocaleTimeString();
    d.appendChild(s);
  }
  chatArea.appendChild(d);
}

function toast(msg) {
  clearTimeout(toastTimer);
  toastEl.textContent = msg;
  toastEl.classList.add('show');
  toastTimer = setTimeout(() => toastEl.classList.remove('show'), 2500);
}

function autoResize(el) {
  const e = el || inputEl;
  e.style.height = 'auto';
  e.style.height = Math.min(e.scrollHeight, 120) + 'px';
}

function fmtCycles(n) {
  if (n >= 1e12) return (n / 1e12).toFixed(2) + 'T';
  if (n >= 1e9)  return (n / 1e9).toFixed(1) + 'B';
  if (n >= 1e6)  return (n / 1e6).toFixed(0) + 'M';
  return n.toString();
}

function handleKey(e) {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendMessage(); }
}

// ── Auth ─────────────────────────────────────────────────────────────
async function initAuth() {
  try {
    authClient = await AuthClient.create();
    if (await authClient.isAuthenticated()) {
      identity = authClient.getIdentity();
      principalId = identity.getPrincipal().toText();
      authProvider = 'ii';
      await createAuthenticatedActor();
      updateAuthUI(true);
    }
  } catch(e) { console.warn('Auth init:', e.message); }
}

async function createAuthenticatedActor() {
  if (!identity) return;
  const agent = new HttpAgent({ identity, host: 'https://icp-api.io' });
  const { idlFactory } = await import("declarations/picoclaw");
  actor = Actor.createActor(idlFactory, {
    agent,
    canisterId: process.env.CANISTER_ID_PICOCLAW,
  });
}

function toggleAuthDropdown() {
  if (identity) { logout(); return; }
  document.getElementById('authDropdown').classList.toggle('show');
}

async function loginII() {
  document.getElementById('authDropdown').classList.remove('show');
  if (!authClient) authClient = await AuthClient.create();
  await new Promise((resolve, reject) => {
    authClient.login({
      identityProvider: 'https://identity.ic0.app',
      maxTimeToLive: BigInt(8) * BigInt(3_600_000_000_000),
      onSuccess: resolve,
      onError: reject,
    });
  });
  identity = authClient.getIdentity();
  principalId = identity.getPrincipal().toText();
  authProvider = 'ii';
  await createAuthenticatedActor();
  updateAuthUI(true);
  toast('II connected: ' + principalId.slice(0, 8) + '...');
  syncSend();
  checkHealth();
}

async function loginPlug() {
  document.getElementById('authDropdown').classList.remove('show');
  if (!window.ic || !window.ic.plug) {
    toast('Plug wallet not found. Install the Plug extension.');
    window.open('https://plugwallet.ooo/', '_blank');
    return;
  }
  try {
    const canisterId = process.env.CANISTER_ID_PICOCLAW;
    await window.ic.plug.requestConnect({ whitelist: [canisterId], host: 'https://icp-api.io' });
    const p = await window.ic.plug.agent.getPrincipal();
    principalId = p.toText();
    identity = { getPrincipal: () => p, _plug: true };
    authProvider = 'plug';
    const { idlFactory } = await import("declarations/picoclaw");
    actor = await window.ic.plug.createActor({ canisterId, interfaceFactory: idlFactory });
    updateAuthUI(true);
    toast('Plug connected: ' + principalId.slice(0, 8) + '...');
    syncSend();
    checkHealth();
  } catch(e) {
    console.error('Plug login failed:', e);
    toast('Plug connection failed');
  }
}

async function logout() {
  if (authProvider === 'plug' && window.ic?.plug) {
    try { await window.ic.plug.disconnect(); } catch(_) {}
  }
  if (authProvider === 'ii' && authClient) {
    await authClient.logout();
  }
  identity = null;
  principalId = null;
  authProvider = null;
  const { picoclaw: anonActor } = await import("declarations/picoclaw");
  actor = anonActor;
  updateAuthUI(false);
  syncSend();
  statusDot.className = 'status-dot';
  chatArea.innerHTML = '<div class="msg system">Session ended. Connect to start chatting.</div>';
  toast('Disconnected');
}

function updateAuthUI(authenticated) {
  if (authenticated && principalId) {
    const short = principalId.slice(0, 5) + '...' + principalId.slice(-3);
    const label = authProvider === 'plug' ? 'Plug' : 'II';
    authBar.innerHTML = `<span class="principal-tag" title="${principalId}">${label}: ${short}</span><button class="auth-btn logout" onclick="window._pc.toggleAuthDropdown()">Disconnect</button>
      <div class="auth-dropdown" id="authDropdown"></div>`;
  } else {
    authBar.innerHTML = `<button class="auth-btn" onclick="window._pc.toggleAuthDropdown()">Connect</button>
      <div class="auth-dropdown" id="authDropdown">
        <button onclick="window._pc.loginII()">Internet Identity</button>
        <button onclick="window._pc.loginPlug()">Plug Wallet</button>
      </div>`;
  }
}

// ── Health / Metrics ─────────────────────────────────────────────────
async function checkHealth() {
  try {
    if (!actor) throw 0;
    await actor.cycle_balance();
    statusDot.className = 'status-dot ok';
    refreshMetrics();
  } catch {
    statusDot.className = 'status-dot err';
  }
}

async function refreshMetrics() {
  if (!actor) return;
  try {
    const [m, bal, q] = await Promise.all([
      actor.get_metrics(),
      actor.cycle_balance(),
      actor.get_queue_length(),
    ]);
    statMsgs.textContent = m.total_messages.toString();
    statCalls.textContent = m.total_calls.toString();
    statQueue.textContent = q.toString();
    statCycles.textContent = (Number(bal) / 1e12).toFixed(2) + 'T';
    const spent = Number(m.total_cycles_spent);
    statSpent.textContent = fmtCycles(spent);
    statSpent.title = m.total_calls > 0
      ? 'Avg: ' + fmtCycles(spent / Number(m.total_calls)) + '/call'
      : '';
    statCost.textContent = '$' + (spent * 1.33 / 1e12).toFixed(4);
  } catch {}
}

// ── History ──────────────────────────────────────────────────────────
async function loadHistory() {
  if (!actor) return;
  try {
    const msgs = await actor.get_history(BigInt(200));
    chatArea.innerHTML = '';
    if (!msgs.length) {
      chatArea.innerHTML = '<div class="msg system">No messages yet. Start chatting!</div>';
    }
    for (let i = 0; i < msgs.length; i++) {
      appendMsg(msgs[i].role, msgs[i].content, msgs[i].timestamp);
    }
    scroll();
  } catch (e) {
    toast('History failed: ' + (e?.message || e));
  }
}

// ── Chat (queue-based — button never blocks) ────────────────────────
function sendMessage() {
  const text = inputEl.value.trim();
  if (!text) return;
  if (!identity) return toast('Connect a wallet first');
  if (!actor) return toast('Not connected');

  inputEl.value = '';
  inputEl.style.height = 'auto';

  appendMsg('user', text);
  scroll();

  queue.push(text);
  drainQueue();
}

async function drainQueue() {
  if (processing) return;
  processing = true;
  stopped = false;
  stopBtn.style.display = 'flex';
  while (queue.length && !stopped) {
    const text = queue.shift();
    chatArea.appendChild(typingEl);
    typingEl.classList.add('show');
    scroll();
    try {
      const r = await actor.chat(text);
      if (stopped) break;
      if (r?.Ok != null) {
        appendMsg('assistant', r.Ok);
      } else if (r?.Err != null) {
        appendMsg('system', 'Error: ' + r.Err);
      } else {
        appendMsg('assistant', String(r ?? ''));
      }
      refreshMetrics();
      if (memPanelOpen) setTimeout(refreshMemory, 1500);
    } catch (e) {
      if (!stopped) appendMsg('system', 'Call failed: ' + (e?.message || e));
    } finally {
      typingEl.classList.remove('show');
      scroll();
    }
  }
  processing = false;
  stopBtn.style.display = 'none';
}

function stopQueue() {
  stopped = true;
  queue.length = 0;
  typingEl.classList.remove('show');
  stopBtn.style.display = 'none';
  toast('Stopped — send a new message anytime');
}

// ── PicoMem ──────────────────────────────────────────────────────────
function toggleMemPanel() {
  memPanelOpen = !memPanelOpen;
  memPanel.classList.toggle('show', memPanelOpen);
  memToggle.classList.toggle('active', memPanelOpen);
  if (memPanelOpen && actor) refreshMemory();
}

function renderTier(elId, sizeId, text, max) {
  const body = document.getElementById(elId);
  const size = document.getElementById(sizeId);
  const len = text ? text.length : 0;
  body.textContent = len ? text : 'empty';
  body.className = len ? 'tier-body' : 'tier-body empty';
  size.textContent = len + '/' + max;
}

function timeAgo(nsTimestamp) {
  const secs = Math.floor((Date.now() - Number(nsTimestamp) / 1e6) / 1000);
  if (secs < 60) return secs + 's ago';
  if (secs < 3600) return Math.floor(secs / 60) + 'm ago';
  return Math.floor(secs / 3600) + 'h ago';
}

async function refreshMemory() {
  if (!actor) return;
  try {
    const [s, webEntries] = await Promise.all([
      actor.get_notes(),
      actor.get_web_memory().catch(() => []),
    ]);
    renderTier('tierI', 'tierISize', s.identity, 256);
    renderTier('tierT', 'tierTSize', s.thread, 600);
    renderTier('tierE', 'tierESize', s.episodes, 900);
    renderTier('tierP', 'tierPSize', s.priors, 128);
    if (s.updated_at > 0n) {
      memTs.textContent = 'updated ' + new Date(Number(s.updated_at) / 1e6).toLocaleTimeString();
    } else {
      memTs.textContent = 'never compressed';
    }
    memStatus.textContent = [s.identity, s.thread, s.episodes, s.priors].filter(x => x.length).length + '/4 tiers';

    // Web memory
    webMemCount.textContent = webEntries.length + '/12';
    if (webEntries.length === 0) {
      webMemBody.textContent = 'no lookups yet';
      webMemBody.className = 'tier-body empty';
    } else {
      const lines = webEntries.map(e => {
        const preview = e.summary.length > 80 ? e.summary.slice(0, 80) + '...' : e.summary;
        return e.url + ' (' + timeAgo(e.timestamp) + '): ' + preview;
      });
      webMemBody.textContent = lines.join('\n');
      webMemBody.className = 'tier-body';
    }
  } catch (e) {
    toast('Memory fetch failed: ' + (e?.message || e));
  }
}

async function triggerCompress() {
  if (!actor || !identity) return toast('Login first');
  const btn = document.getElementById('compressBtn');
  btn.disabled = true;
  btn.textContent = 'Compressing...';
  try {
    const r = await actor.compress_context();
    if (r?.Ok != null) { toast('Compression complete'); await refreshMemory(); }
    else toast('Compress error: ' + (r?.Err || 'unknown'));
  } catch (e) {
    toast('Compress failed: ' + (e?.message || e));
  }
  btn.disabled = false;
  btn.textContent = 'Compress Now';
}

async function clearMemory() {
  if (!actor || !identity) return toast('Login first');
  if (!confirm('Clear all PicoMem tiers? This cannot be undone.')) return;
  try {
    const [r, w] = await Promise.all([
      actor.clear_notes(),
      actor.clear_web_memory().catch(() => ({ Ok: null })),
    ]);
    if (r?.Ok != null) { toast('Memory cleared'); await refreshMemory(); }
    else toast('Clear error: ' + (r?.Err || 'unknown'));
  } catch (e) {
    toast('Clear failed: ' + (e?.message || e));
  }
}

// ── Wire up ─────────────────────────────────────────────────────────
window._pc = {
  sendMessage, loadHistory, handleKey, autoResize, stopQueue,
  toggleAuthDropdown, loginII, loginPlug,
  toggleMemPanel, refreshMemory, triggerCompress, clearMemory,
};

// ── Init ─────────────────────────────────────────────────────────────
await initAuth();
syncSend();
checkHealth();
if (identity) {
  refreshMemory();
} else {
  chatArea.innerHTML = '<div class="msg system">Connect a wallet to start chatting.</div>';
}
setInterval(refreshMetrics, 30000);
